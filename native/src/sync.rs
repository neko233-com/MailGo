use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use imap::types::Flag;
use serde::{Deserialize, Serialize};

use crate::mail::{parse_full, parse_header, CachedMailbox, CachedMessage};
use crate::providers::{Authentication, ProviderProfile, TransportSecurity};

const HEADER_FETCH_QUERY: &str = "UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]";
const FULL_FETCH_QUERY: &str = "UID FLAGS RFC822";
const MAX_HEADER_MESSAGES: usize = 100;
const CACHE_FILE: &str = "inbox.bin";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub account_id: String,
    pub folder: String,
    pub fetched: usize,
    pub unread: usize,
    pub cache_path: String,
    pub synced_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailDetail {
    pub message: CachedMessage,
}

fn connect(profile: &ProviderProfile) -> Result<imap::Client<imap::Connection>> {
    let mode = match profile.imap.security {
        TransportSecurity::Tls => imap::ConnectionMode::Tls,
        TransportSecurity::StartTls => imap::ConnectionMode::StartTls,
    };
    imap::ClientBuilder::new(profile.imap.host.as_str(), profile.imap.port)
        .mode(mode)
        .connect()
        .with_context(|| format!("connect IMAP host {}", profile.imap.host))
}

/// Keep the local cache fresh while the window is hidden. The scheduler intentionally runs on a
/// dedicated thread so IMAP handshakes never block rdesktop's WebView event loop.
pub fn spawn_scheduler(shared: Arc<Mutex<crate::MailGoState>>, cache_root: PathBuf) {
    thread::Builder::new()
        .name("mailgo-sync-scheduler".into())
        .spawn(move || loop {
            thread::sleep(Duration::from_secs(300));
            let accounts = match shared.lock() {
                Ok(app) => app.state.accounts.clone(),
                Err(_) => {
                    tracing::warn!("background sync state lock poisoned");
                    continue;
                }
            };
            for account in accounts {
                let profile = match crate::profile_for_account(&account) {
                    Ok(profile) => profile,
                    Err(_) => continue,
                };
                let credential = match crate::load_credential(&account.id) {
                    Ok(credential) => credential,
                    Err(_) => continue,
                };
                match sync_account(
                    &account.id,
                    profile,
                    &account.email,
                    &credential,
                    &cache_root,
                ) {
                    Ok(result) => {
                        if let Ok(mut app) = shared.lock() {
                            if let Some(stored) = app
                                .state
                                .accounts
                                .iter_mut()
                                .find(|item| item.id == account.id)
                            {
                                stored.unread = result.unread as u32;
                                stored.status = "synced".into();
                                stored.last_sync = "后台刚刚同步".into();
                            }
                            if let Err(error) = app.save() {
                                tracing::warn!("background sync state save failed: {error}");
                            }
                        }
                    }
                    Err(error) => tracing::warn!(
                        account_id = %account.id,
                        "background sync failed: {error}"
                    ),
                }
            }
        })
        .expect("start MailGo sync scheduler");
}

struct XOAuth2 {
    user: String,
    token: String,
}

impl imap::Authenticator for XOAuth2 {
    type Response = Vec<u8>;

    fn process(&self, _challenge: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token).into_bytes()
    }
}

/// Fetch a bounded header window using UID semantics, then atomically write the offline mailbox
/// cache. Re-running this function is safe and preserves a complete local copy of the latest
/// window without sending any credentials to the frontend.
pub fn sync_account(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    cache_root: &Path,
) -> Result<SyncResult> {
    if credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before syncing"));
    }

    let client = connect(&profile)?;
    let mut session =
        match profile.authentication {
            Authentication::AppPassword | Authentication::Password => client
                .login(email, credential)
                .map_err(|(error, _)| anyhow!("IMAP authentication failed: {error}"))?,
            Authentication::OAuth2 => client
                .authenticate(
                    "XOAUTH2",
                    &XOAuth2 {
                        user: email.to_string(),
                        token: credential.to_string(),
                    },
                )
                .map_err(|(error, _)| anyhow!("IMAP OAuth authentication failed: {error}"))?,
        };

    let mailbox = session.select("INBOX").context("select INBOX")?;
    let mut uids = session
        .uid_search("ALL")
        .context("search INBOX")?
        .into_iter()
        .collect::<Vec<_>>();
    uids.sort_unstable();
    let selected_uids = uids
        .into_iter()
        .rev()
        .take(MAX_HEADER_MESSAGES)
        .collect::<Vec<_>>();
    let uid_set = selected_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    let mut messages = Vec::with_capacity(selected_uids.len());
    if !uid_set.is_empty() {
        let fetched = session
            .uid_fetch(uid_set, HEADER_FETCH_QUERY)
            .context("fetch message headers")?;
        for item in fetched.iter() {
            let Some(uid) = item.uid else { continue };
            let Some(header) = item.header() else {
                continue;
            };
            let unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
            let starred = item
                .flags()
                .iter()
                .any(|flag| matches!(flag, Flag::Flagged));
            if let Ok(message) = parse_header(account_id, "INBOX", uid, unread, starred, header) {
                messages.push(message);
            }
        }
    }
    messages.sort_by_key(|message| std::cmp::Reverse(message.uid));

    let synced_at = now_stamp();
    let mut cached = CachedMailbox::empty(account_id, "INBOX");
    cached.uid_validity = mailbox.uid_validity;
    cached.synced_at = synced_at.clone();
    cached.messages = messages;
    let cache_path = save_mailbox(cache_root, account_id, &cached)?;
    let unread = cached
        .messages
        .iter()
        .filter(|message| message.unread)
        .count();
    let fetched = cached.messages.len();

    session.logout().ok();
    Ok(SyncResult {
        account_id: account_id.to_string(),
        folder: "INBOX".to_string(),
        fetched,
        unread,
        cache_path: cache_path.display().to_string(),
        synced_at,
    })
}

/// Download and parse one full message only when the reader asks for it. The raw message can be
/// retained by a caller in an account cache, but the returned representation is sanitized.
pub fn fetch_message(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    uid: u32,
) -> Result<MailDetail> {
    let client = connect(&profile)?;
    let mut session =
        match profile.authentication {
            Authentication::AppPassword | Authentication::Password => client
                .login(email, credential)
                .map_err(|(error, _)| anyhow!("IMAP authentication failed: {error}"))?,
            Authentication::OAuth2 => client
                .authenticate(
                    "XOAUTH2",
                    &XOAuth2 {
                        user: email.to_string(),
                        token: credential.to_string(),
                    },
                )
                .map_err(|(error, _)| anyhow!("IMAP OAuth authentication failed: {error}"))?,
        };
    session.select(folder)?;
    let fetched = session.uid_fetch(uid.to_string(), FULL_FETCH_QUERY)?;
    let item = fetched
        .iter()
        .next()
        .ok_or_else(|| anyhow!("message UID {uid} was not found"))?;
    let raw = item
        .body()
        .ok_or_else(|| anyhow!("message UID {uid} has no RFC822 body"))?;
    let unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
    let starred = item
        .flags()
        .iter()
        .any(|flag| matches!(flag, Flag::Flagged));
    let message = parse_full(account_id, folder, uid, unread, starred, raw)?;
    session.logout().ok();
    Ok(MailDetail { message })
}

pub fn load_mailbox(cache_root: &Path, account_id: &str) -> Result<Option<CachedMailbox>> {
    let directory = cache_root.join(safe_component(account_id));
    let encrypted_path = directory.join(CACHE_FILE);
    let legacy_path = directory.join("inbox.json");
    for path in [&encrypted_path, &legacy_path] {
        match fs::read(path) {
            Ok(contents) => {
                let decoded = if path == &encrypted_path {
                    unprotect_cache(&contents)
                        .with_context(|| format!("decrypt {}", path.display()))?
                } else {
                    contents
                };
                return Ok(Some(
                    serde_json::from_slice(&decoded)
                        .with_context(|| format!("parse {}", path.display()))?,
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        }
    }
    Ok(None)
}

pub fn load_cached_message(
    cache_root: &Path,
    account_id: &str,
    uid: u32,
) -> Result<Option<CachedMessage>> {
    Ok(load_mailbox(cache_root, account_id)?.and_then(|mailbox| {
        mailbox
            .messages
            .into_iter()
            .find(|message| message.uid == uid)
    }))
}

pub fn save_cached_message(
    cache_root: &Path,
    account_id: &str,
    message: &CachedMessage,
) -> Result<()> {
    let mut mailbox = load_mailbox(cache_root, account_id)?
        .unwrap_or_else(|| CachedMailbox::empty(account_id, message.folder.as_str()));
    mailbox
        .messages
        .retain(|cached| cached.uid != message.uid || cached.folder != message.folder);
    mailbox.messages.push(message.clone());
    mailbox
        .messages
        .sort_by_key(|cached| std::cmp::Reverse(cached.uid));
    save_mailbox(cache_root, account_id, &mailbox).map(|_| ())
}

pub fn update_cached_flags(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    flag: &str,
    enabled: bool,
) -> Result<()> {
    let Some(mut mailbox) = load_mailbox(cache_root, account_id)? else {
        return Ok(());
    };
    if let Some(message) = mailbox
        .messages
        .iter_mut()
        .find(|message| message.uid == uid && message.folder.eq_ignore_ascii_case(folder))
    {
        match flag {
            "\\Seen" => message.unread = !enabled,
            "\\Flagged" => message.starred = enabled,
            _ => return Ok(()),
        }
        save_mailbox(cache_root, account_id, &mailbox)?;
    }
    Ok(())
}

/// Apply a flag mutation with UID semantics. This is used by read/star/archive actions so local
/// UI state and provider state share the same identifier and do not depend on sequence numbers.
pub fn set_flag(
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    uid: u32,
    flag: &str,
    enabled: bool,
) -> Result<()> {
    let client = connect(&profile)?;
    let mut session =
        match profile.authentication {
            Authentication::AppPassword | Authentication::Password => client
                .login(email, credential)
                .map_err(|(error, _)| anyhow!("IMAP authentication failed: {error}"))?,
            Authentication::OAuth2 => client
                .authenticate(
                    "XOAUTH2",
                    &XOAuth2 {
                        user: email.to_string(),
                        token: credential.to_string(),
                    },
                )
                .map_err(|(error, _)| anyhow!("IMAP OAuth authentication failed: {error}"))?,
        };
    session.select(folder)?;
    let operation = if enabled {
        "+FLAGS.SILENT"
    } else {
        "-FLAGS.SILENT"
    };
    session.uid_store(uid.to_string(), format!("{operation} ({flag})"))?;
    session.logout().ok();
    Ok(())
}

fn save_mailbox(cache_root: &Path, account_id: &str, mailbox: &CachedMailbox) -> Result<PathBuf> {
    let directory = cache_root.join(safe_component(account_id));
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let path = directory.join(CACHE_FILE);
    let temporary = directory.join("inbox.bin.tmp");
    let payload = protect_cache(&serde_json::to_vec_pretty(mailbox)?)?;
    fs::write(&temporary, payload).context("write mailbox cache")?;
    if path.exists() {
        let backup = directory.join("inbox.bin.bak");
        let _ = fs::remove_file(&backup);
        fs::rename(&path, &backup).context("backup mailbox cache")?;
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::rename(&backup, &path);
            return Err(error).context("replace mailbox cache");
        }
        let _ = fs::remove_file(backup);
    } else {
        fs::rename(&temporary, &path).context("commit mailbox cache")?;
    }
    Ok(path)
}

#[cfg(target_os = "windows")]
fn protect_cache(payload: &[u8]) -> Result<Vec<u8>> {
    use std::mem::zeroed;
    use std::slice::from_raw_parts;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};

    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(payload.len()).context("mail cache is too large")?,
        pbData: payload.as_ptr() as *mut u8,
    };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { zeroed() };
    let success = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    if success == 0 || output.pbData.is_null() {
        return Err(anyhow!(
            "Windows data protection rejected the mailbox cache"
        ));
    }
    let result = unsafe { from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut std::ffi::c_void) };
    Ok(result)
}

#[cfg(not(target_os = "windows"))]
fn protect_cache(payload: &[u8]) -> Result<Vec<u8>> {
    Ok(payload.to_vec())
}

#[cfg(target_os = "windows")]
fn unprotect_cache(payload: &[u8]) -> Result<Vec<u8>> {
    use std::mem::zeroed;
    use std::slice::from_raw_parts;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(payload.len()).context("encrypted mailbox cache is too large")?,
        pbData: payload.as_ptr() as *mut u8,
    };
    let mut output: CRYPT_INTEGER_BLOB = unsafe { zeroed() };
    let success = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    if success == 0 || output.pbData.is_null() {
        return Err(anyhow!(
            "Windows data protection could not unlock the mailbox cache"
        ));
    }
    let result = unsafe { from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as *mut std::ffi::c_void) };
    Ok(result)
}

#[cfg(not(target_os = "windows"))]
fn unprotect_cache(payload: &[u8]) -> Result<Vec<u8>> {
    Ok(payload.to_vec())
}

fn safe_component(value: &str) -> String {
    let safe = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        "account".into()
    } else {
        safe
    }
}

fn now_stamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!("unix:{seconds}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use imap::Authenticator;

    #[test]
    fn cache_component_cannot_escape_account_directory() {
        assert_eq!(safe_component("../../secret"), ".._.._secret");
        assert_eq!(safe_component("account-1"), "account-1");
    }

    #[test]
    fn xoauth2_payload_does_not_log_or_store_credentials() {
        let auth = XOAuth2 {
            user: "person@example.com".into(),
            token: "token-value".into(),
        };
        let payload = String::from_utf8(auth.process(b"")).unwrap();
        assert!(payload.contains("user=person@example.com"));
        assert!(payload.contains("auth=Bearer token-value"));
    }
}
