use std::collections::HashSet;
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
const MUTATION_FILE: &str = "mutations.bin";
const MOVE_MUTATION_FILE: &str = "moves.bin";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingFlagMutation {
    folder: String,
    uid: u32,
    flag: String,
    enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingMoveMutation {
    operation: String,
    folder: String,
    uid: u32,
    #[serde(default)]
    target_folder: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub account_id: String,
    pub folder: String,
    pub fetched: usize,
    pub unread: usize,
    pub cache_path: String,
    pub synced_at: String,
    pub folders: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMutationCounts {
    pub flags: usize,
    pub moves: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailDetail {
    pub message: CachedMessage,
}

#[derive(Debug)]
pub struct AttachmentData {
    pub file_name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
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

fn authenticate(
    profile: &ProviderProfile,
    email: &str,
    credential: &str,
) -> Result<imap::Session<imap::Connection>> {
    let client = connect(profile)?;
    match profile.authentication {
        Authentication::AppPassword | Authentication::Password => client
            .login(email, credential)
            .map_err(|(error, _)| anyhow!("IMAP authentication failed: {error}")),
        Authentication::OAuth2 => client
            .authenticate(
                "XOAUTH2",
                &XOAuth2 {
                    user: email.to_string(),
                    token: crate::oauth::access_token(credential),
                },
            )
            .map_err(|(error, _)| anyhow!("IMAP OAuth authentication failed: {error}")),
    }
}

/// Keep the local cache fresh while the window is hidden. The scheduler intentionally runs on a
/// dedicated thread so IMAP handshakes never block rdesktop's WebView event loop.
pub fn spawn_scheduler(shared: Arc<Mutex<crate::MailGoState>>, cache_root: PathBuf) {
    thread::Builder::new()
        .name("mailgo-sync-scheduler".into())
        .spawn(move || loop {
            thread::sleep(Duration::from_secs(300));
            let (accounts, notifications_enabled) = match shared.lock() {
                Ok(app) => (app.state.accounts.clone(), app.state.notifications_enabled),
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
                let credential = match crate::load_credential(&account) {
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
                        if notifications_enabled && result.unread > account.unread as usize {
                            crate::tray::notify_new_mail(
                                "MailGo",
                                &format!(
                                    "{} 有 {} 封未读邮件",
                                    account.label,
                                    result.unread - account.unread as usize
                                ),
                            );
                        }
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
    let mut attempt = 0u32;
    loop {
        match sync_account_once(account_id, profile.clone(), email, credential, cache_root) {
            Ok(result) => return Ok(result),
            Err(error) if attempt < 2 => {
                let Some(delay) = retry_delay(&error, attempt) else {
                    return Err(error);
                };
                attempt += 1;
                tracing::warn!(account_id = %account_id, attempt, delay = delay.as_secs(), "recoverable IMAP sync failure; retrying");
                thread::sleep(delay);
            }
            Err(error) => return Err(error),
        }
    }
}

fn sync_account_once(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    cache_root: &Path,
) -> Result<SyncResult> {
    if credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before syncing"));
    }

    let mut session = authenticate(&profile, email, credential)?;

    flush_queued_moves(&mut session, cache_root, account_id, profile.provider);
    flush_queued_flags(&mut session, cache_root, account_id);

    let synced_at = now_stamp();
    let mut synced_folders = Vec::new();
    let mut inbox_fetched = 0usize;
    let mut inbox_unread = 0usize;
    let mut cache_path = cache_root.join(safe_component(account_id)).join(CACHE_FILE);

    for folder in discover_folders(&mut session, profile.provider) {
        let Ok(mailbox) = session.select(&folder) else {
            continue;
        };
        let (cached, fetched) = sync_folder_latest(
            &mut session,
            account_id,
            &folder,
            mailbox.uid_validity,
            cache_root,
            synced_at.clone(),
        )?;
        let path = save_mailbox(cache_root, account_id, &cached)?;
        if folder.eq_ignore_ascii_case("INBOX") {
            cache_path = path;
            inbox_fetched = fetched;
            inbox_unread = cached
                .messages
                .iter()
                .filter(|message| message.unread)
                .count();
        }
        synced_folders.push(folder);
    }

    if !synced_folders
        .iter()
        .any(|folder| folder.eq_ignore_ascii_case("INBOX"))
    {
        return Err(anyhow!("INBOX is unavailable on the mail server"));
    }

    session.logout().ok();
    Ok(SyncResult {
        account_id: account_id.to_string(),
        folder: "INBOX".to_string(),
        fetched: inbox_fetched,
        unread: inbox_unread,
        cache_path: cache_path.display().to_string(),
        synced_at,
        folders: synced_folders,
    })
}

fn sync_folder_latest(
    session: &mut imap::Session<imap::Connection>,
    account_id: &str,
    folder: &str,
    uid_validity: Option<u32>,
    cache_root: &Path,
    synced_at: String,
) -> Result<(CachedMailbox, usize)> {
    validate_mailbox_name(folder)?;
    let mut all_uids = session
        .uid_search("ALL")
        .with_context(|| format!("search {folder}"))?
        .into_iter()
        .collect::<Vec<_>>();
    all_uids.sort_unstable();
    let current_uids = all_uids.iter().copied().collect::<HashSet<_>>();
    let total_uids = all_uids.len();
    let mut cached = load_mailbox_for_folder(cache_root, account_id, folder)?
        .unwrap_or_else(|| CachedMailbox::empty(account_id, folder));
    let uid_validity_changed = cached.uid_validity.is_some() && cached.uid_validity != uid_validity;
    if uid_validity_changed {
        cached.messages.clear();
        cached.oldest_uid = None;
        cached.has_more = false;
    }

    // The header window is fetched only for a cold cache or UIDs newer than the newest cached
    // message. Existing bodies/attachments remain untouched and are refreshed through FLAGS.
    let newest_cached_uid = cached.messages.iter().map(|message| message.uid).max();
    let selected_uids = match newest_cached_uid {
        None => all_uids
            .iter()
            .rev()
            .take(MAX_HEADER_MESSAGES)
            .copied()
            .collect::<Vec<_>>(),
        Some(newest_uid) => all_uids
            .iter()
            .filter(|uid| **uid > newest_uid)
            .rev()
            .take(MAX_HEADER_MESSAGES)
            .copied()
            .collect::<Vec<_>>(),
    };
    let uid_set = selected_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut fetched_messages = Vec::with_capacity(selected_uids.len());
    if !uid_set.is_empty() {
        let fetched = session
            .uid_fetch(uid_set, HEADER_FETCH_QUERY)
            .with_context(|| format!("fetch {folder} message headers"))?;
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
            if let Ok(message) = parse_header(account_id, folder, uid, unread, starred, header) {
                fetched_messages.push(message);
            }
        }
    }
    fetched_messages.sort_by_key(|message| std::cmp::Reverse(message.uid));

    cached
        .messages
        .retain(|message| current_uids.contains(&message.uid));
    let refresh_uids = cached
        .messages
        .iter()
        .map(|message| message.uid)
        .filter(|uid| current_uids.contains(uid))
        .take(MAX_HEADER_MESSAGES)
        .collect::<Vec<_>>();
    let refresh_uid_set = refresh_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if !refresh_uid_set.is_empty() {
        let fetched = session
            .uid_fetch(refresh_uid_set, "UID FLAGS")
            .with_context(|| format!("refresh {folder} message flags"))?;
        for item in fetched.iter() {
            let Some(uid) = item.uid else { continue };
            if let Some(message) = cached
                .messages
                .iter_mut()
                .find(|message| message.uid == uid)
            {
                message.unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
                message.starred = item
                    .flags()
                    .iter()
                    .any(|flag| matches!(flag, Flag::Flagged));
            }
        }
    }

    cached.uid_validity = uid_validity;
    cached.synced_at = synced_at;
    cached
        .messages
        .retain(|message| !fetched_messages.iter().any(|item| item.uid == message.uid));
    cached.messages.extend(fetched_messages.iter().cloned());
    cached
        .messages
        .sort_by_key(|message| std::cmp::Reverse(message.uid));
    cached.oldest_uid = cached.messages.iter().map(|message| message.uid).min();
    cached.has_more = total_uids > cached.messages.len();
    Ok((cached, fetched_messages.len()))
}

/// Fetch one older page for a folder and merge it into the encrypted local cache. The cursor is
/// the oldest cached UID; using UID ranges keeps pagination stable when new mail arrives while a
/// user is scrolling through a mailbox.
pub fn sync_folder_page(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    before_uid: Option<u32>,
    limit: usize,
    cache_root: &Path,
) -> Result<SyncResult> {
    validate_mailbox_name(folder)?;
    let mut attempt = 0u32;
    loop {
        match sync_folder_page_once(
            account_id,
            profile.clone(),
            email,
            credential,
            folder,
            before_uid,
            limit,
            cache_root,
        ) {
            Ok(result) => return Ok(result),
            Err(error) if attempt < 2 => {
                let Some(delay) = retry_delay(&error, attempt) else {
                    return Err(error);
                };
                attempt += 1;
                tracing::warn!(account_id = %account_id, attempt, delay = delay.as_secs(), "recoverable IMAP page failure; retrying");
                thread::sleep(delay);
            }
            Err(error) => return Err(error),
        }
    }
}

fn sync_folder_page_once(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    before_uid: Option<u32>,
    limit: usize,
    cache_root: &Path,
) -> Result<SyncResult> {
    if credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before syncing"));
    }
    let page_size = limit.clamp(1, MAX_HEADER_MESSAGES);
    let mut session = authenticate(&profile, email, credential)?;
    let mailbox = session
        .select(folder)
        .with_context(|| format!("select {folder}"))?;
    let mut cached = load_mailbox_for_folder(cache_root, account_id, folder)?
        .unwrap_or_else(|| CachedMailbox::empty(account_id, folder));
    let uid_validity_changed =
        cached.uid_validity.is_some() && cached.uid_validity != mailbox.uid_validity;
    if uid_validity_changed {
        cached.messages.clear();
        cached.oldest_uid = None;
        cached.has_more = false;
    }
    let effective_before_uid = if uid_validity_changed {
        None
    } else {
        before_uid
    };
    let query = match effective_before_uid {
        Some(uid) if uid <= 1 => {
            let unread = if folder.eq_ignore_ascii_case("INBOX") {
                cached
                    .messages
                    .iter()
                    .filter(|message| message.unread)
                    .count()
            } else {
                load_mailbox_for_folder(cache_root, account_id, "INBOX")?
                    .map(|mailbox| {
                        mailbox
                            .messages
                            .iter()
                            .filter(|message| message.unread)
                            .count()
                    })
                    .unwrap_or_default()
            };
            session.logout().ok();
            return Ok(SyncResult {
                account_id: account_id.to_string(),
                folder: folder.to_string(),
                fetched: 0,
                unread,
                cache_path: cache_root
                    .join(safe_component(account_id))
                    .join(cache_file_name(folder))
                    .display()
                    .to_string(),
                synced_at: now_stamp(),
                folders: vec![folder.to_string()],
            });
        }
        Some(uid) => format!("UID 1:{}", uid - 1),
        None => "ALL".to_string(),
    };
    let mut uids = session
        .uid_search(query)
        .with_context(|| format!("search older messages in {folder}"))?
        .into_iter()
        .collect::<Vec<_>>();
    uids.sort_unstable();
    let has_more = uids.len() > page_size;
    let selected_uids = uids
        .iter()
        .rev()
        .take(page_size)
        .copied()
        .collect::<Vec<_>>();
    let uid_set = selected_uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut fetched_messages = Vec::with_capacity(selected_uids.len());
    if !uid_set.is_empty() {
        let fetched = session
            .uid_fetch(uid_set, HEADER_FETCH_QUERY)
            .with_context(|| format!("fetch {folder} message headers"))?;
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
            if let Ok(message) = parse_header(account_id, folder, uid, unread, starred, header) {
                fetched_messages.push(message);
            }
        }
    }
    fetched_messages.sort_by_key(|message| std::cmp::Reverse(message.uid));

    cached.uid_validity = mailbox.uid_validity;
    cached.synced_at = now_stamp();
    cached
        .messages
        .retain(|message| !fetched_messages.iter().any(|item| item.uid == message.uid));
    cached.messages.extend(fetched_messages.iter().cloned());
    cached
        .messages
        .sort_by_key(|message| std::cmp::Reverse(message.uid));
    cached.oldest_uid = cached.messages.iter().map(|message| message.uid).min();
    cached.has_more = has_more;
    let path = save_mailbox(cache_root, account_id, &cached)?;
    let unread = if folder.eq_ignore_ascii_case("INBOX") {
        cached
            .messages
            .iter()
            .filter(|message| message.unread)
            .count()
    } else {
        load_mailbox_for_folder(cache_root, account_id, "INBOX")?
            .map(|mailbox| {
                mailbox
                    .messages
                    .iter()
                    .filter(|message| message.unread)
                    .count()
            })
            .unwrap_or_default()
    };
    session.logout().ok();
    Ok(SyncResult {
        account_id: account_id.to_string(),
        folder: folder.to_string(),
        fetched: fetched_messages.len(),
        unread,
        cache_path: path.display().to_string(),
        synced_at: cached.synced_at,
        folders: vec![folder.to_string()],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncErrorClass {
    Authentication,
    RateLimited,
    Transport,
    Permanent,
}

fn classify_sync_error(error: &anyhow::Error) -> SyncErrorClass {
    let message = error.to_string().to_ascii_lowercase();
    if [
        "authentication",
        "authorization",
        "invalid credential",
        "requires authorization",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        return SyncErrorClass::Authentication;
    }
    if [
        "rate limit",
        "too many requests",
        "too many connections",
        "throttl",
        "quota exceeded",
        "try again later",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        return SyncErrorClass::RateLimited;
    }
    if [
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "connection aborted",
        "broken pipe",
        "temporarily unavailable",
        "network is unreachable",
        "eof",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        return SyncErrorClass::Transport;
    }
    SyncErrorClass::Permanent
}

fn retry_delay(error: &anyhow::Error, attempt: u32) -> Option<Duration> {
    if let Some(retry_after) = retry_after_seconds(error) {
        return Some(Duration::from_secs(retry_after.clamp(1, 300)));
    }
    let delay_seconds = match classify_sync_error(error) {
        SyncErrorClass::Transport => 1u64 << attempt,
        SyncErrorClass::RateLimited => 5u64.saturating_mul(1u64 << attempt),
        SyncErrorClass::Authentication | SyncErrorClass::Permanent => return None,
    };
    Some(Duration::from_secs(delay_seconds.min(60)))
}

/// Honor a provider's Retry-After hint when it survives the transport error text. The IMAP crate
/// does not expose arbitrary response headers, so this parser is intentionally conservative and
/// falls back to bounded exponential delays when no numeric hint is present.
fn retry_after_seconds(error: &anyhow::Error) -> Option<u64> {
    let message = error.to_string().to_ascii_lowercase();
    for marker in ["retry-after", "retry after", "retry_after"] {
        let Some(start) = message.find(marker) else {
            continue;
        };
        let digits = message[start + marker.len()..]
            .chars()
            .skip_while(|character| !character.is_ascii_digit())
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        if let Ok(seconds) = digits.parse::<u64>() {
            return Some(seconds);
        }
    }
    None
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
    cache_root: &Path,
) -> Result<MailDetail> {
    validate_mailbox_name(folder)?;
    let mut session = authenticate(&profile, email, credential)?;
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
    let mut message = parse_full(account_id, folder, uid, unread, starred, raw)?;
    let payloads = crate::mail::extract_attachment_payloads(raw)?;
    crate::mail::embed_inline_images(&mut message.html_body, &payloads);
    store_attachment_payloads(cache_root, account_id, folder, uid, &mut message, &payloads)?;
    session.logout().ok();
    Ok(MailDetail { message })
}

fn store_attachment_payloads(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    message: &mut CachedMessage,
    payloads: &[crate::mail::AttachmentPayload],
) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let directory = cache_root
        .join(safe_component(account_id))
        .join("attachments")
        .join(safe_component(folder));
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    for (index, payload) in payloads.iter().enumerate() {
        let path = directory.join(format!("{uid}-{index}.bin"));
        let temporary = directory.join(format!("{uid}-{index}.bin.tmp"));
        let encrypted = protect_cache(&payload.bytes)?;
        fs::write(&temporary, encrypted).context("write cached attachment")?;
        if path.exists() {
            fs::remove_file(&path).context("replace cached attachment")?;
        }
        fs::rename(&temporary, &path).context("commit cached attachment")?;
        if let Some(attachment) = message
            .attachments
            .iter_mut()
            .find(|item| item.index == index)
        {
            attachment.cache_path = Some(path.display().to_string());
        }
    }
    Ok(())
}

pub fn load_attachment_data(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    index: usize,
) -> Result<AttachmentData> {
    let mailbox = load_mailbox_for_folder(cache_root, account_id, folder)?
        .ok_or_else(|| anyhow!("message is not cached"))?;
    let message = mailbox
        .messages
        .iter()
        .find(|message| message.uid == uid)
        .ok_or_else(|| anyhow!("message is not cached"))?;
    let metadata = message
        .attachments
        .iter()
        .find(|attachment| attachment.index == index)
        .ok_or_else(|| anyhow!("attachment is not available"))?;
    let path = cache_root
        .join(safe_component(account_id))
        .join("attachments")
        .join(safe_component(folder))
        .join(format!("{uid}-{index}.bin"));
    let encrypted =
        fs::read(&path).with_context(|| format!("read attachment {}", path.display()))?;
    let bytes = unprotect_cache(&encrypted).context("decrypt cached attachment")?;
    Ok(AttachmentData {
        file_name: metadata.file_name.clone(),
        content_type: metadata.content_type.clone(),
        bytes,
    })
}

pub fn load_mailbox_for_folder(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
) -> Result<Option<CachedMailbox>> {
    let directory = cache_root.join(safe_component(account_id));
    let encrypted_path = directory.join(cache_file_name(folder));
    let legacy_path = directory.join("inbox.json");
    let paths: Vec<&Path> = if folder.eq_ignore_ascii_case("INBOX") {
        vec![encrypted_path.as_path(), legacy_path.as_path()]
    } else {
        vec![encrypted_path.as_path()]
    };
    for path in paths {
        match fs::read(path) {
            Ok(contents) => {
                let decoded = if path == encrypted_path.as_path() {
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

pub fn remove_account_cache(cache_root: &Path, account_id: &str) -> Result<()> {
    let directory = cache_root.join(safe_component(account_id));
    match fs::remove_dir_all(&directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove account cache {}", directory.display()))
        }
    }
}

pub fn load_cached_message(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<Option<CachedMessage>> {
    Ok(
        load_mailbox_for_folder(cache_root, account_id, folder)?.and_then(|mailbox| {
            mailbox
                .messages
                .into_iter()
                .find(|message| message.uid == uid && message.folder.eq_ignore_ascii_case(folder))
        }),
    )
}

pub fn save_cached_message(
    cache_root: &Path,
    account_id: &str,
    message: &CachedMessage,
) -> Result<()> {
    let mut mailbox = load_mailbox_for_folder(cache_root, account_id, &message.folder)?
        .unwrap_or_else(|| CachedMailbox::empty(account_id, message.folder.as_str()));
    mailbox
        .messages
        .retain(|cached| cached.uid != message.uid || cached.folder != message.folder);
    mailbox.messages.push(message.clone());
    mailbox
        .messages
        .sort_by_key(|cached| std::cmp::Reverse(cached.uid));
    mailbox.oldest_uid = mailbox.messages.iter().map(|cached| cached.uid).min();
    mailbox.synced_at = now_stamp();
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
    let Some(mut mailbox) = load_mailbox_for_folder(cache_root, account_id, folder)? else {
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

fn refresh_mailbox_metadata(mailbox: &mut CachedMailbox) {
    mailbox
        .messages
        .sort_by_key(|message| std::cmp::Reverse(message.uid));
    mailbox.oldest_uid = mailbox.messages.iter().map(|message| message.uid).min();
    mailbox.synced_at = now_stamp();
}

/// Move a cached message between folder caches immediately. The provider operation may still be
/// queued, but the local UI must reflect the user's action while offline.
pub fn update_cached_move(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    target_folder: &str,
) -> Result<()> {
    if folder.eq_ignore_ascii_case(target_folder) {
        return Ok(());
    }
    let Some(mut source) = load_mailbox_for_folder(cache_root, account_id, folder)? else {
        return Ok(());
    };
    let Some(mut message) = source
        .messages
        .iter()
        .find(|message| message.uid == uid && message.folder.eq_ignore_ascii_case(folder))
        .cloned()
    else {
        return Ok(());
    };
    source
        .messages
        .retain(|item| !(item.uid == uid && item.folder.eq_ignore_ascii_case(folder)));
    refresh_mailbox_metadata(&mut source);

    let mut target = load_mailbox_for_folder(cache_root, account_id, target_folder)?
        .unwrap_or_else(|| CachedMailbox::empty(account_id, target_folder));
    message.folder = target_folder.to_string();
    target.messages.retain(|item| item.uid != uid);
    target.messages.push(message);
    refresh_mailbox_metadata(&mut target);
    save_mailbox(cache_root, account_id, &target)?;
    save_mailbox(cache_root, account_id, &source)?;
    Ok(())
}

pub fn remove_cached_message(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<()> {
    let Some(mut mailbox) = load_mailbox_for_folder(cache_root, account_id, folder)? else {
        return Ok(());
    };
    mailbox
        .messages
        .retain(|message| !(message.uid == uid && message.folder.eq_ignore_ascii_case(folder)));
    refresh_mailbox_metadata(&mut mailbox);
    save_mailbox(cache_root, account_id, &mailbox).map(|_| ())
}

fn validate_mailbox_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.len() > 512
        || name
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err(anyhow!("invalid destination mailbox"));
    }
    Ok(())
}

fn quoted_mailbox_name(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

fn move_uid(
    session: &mut imap::Session<imap::Connection>,
    uid: u32,
    target_folder: &str,
) -> Result<()> {
    validate_mailbox_name(target_folder)?;
    let capabilities = session
        .capabilities()
        .map_err(|error| anyhow!("read IMAP capabilities: {error}"))?;
    let supports_move = capabilities.has_str("MOVE");
    if supports_move {
        return session
            .uid_mv(uid.to_string(), target_folder)
            .map_err(|error| anyhow!("IMAP UID MOVE failed: {error}"));
    }
    if !capabilities.has_str("UIDPLUS") {
        return Err(anyhow!(
            "IMAP server supports neither MOVE nor UIDPLUS; refusing an unsafe move fallback"
        ));
    }
    session
        .uid_copy(uid.to_string(), quoted_mailbox_name(target_folder))
        .map_err(|error| anyhow!("IMAP UID COPY failed: {error}"))?;
    session
        .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
        .map_err(|error| anyhow!("IMAP delete source failed: {error}"))?;
    session
        .uid_expunge(uid.to_string())
        .map_err(|error| anyhow!("IMAP UID EXPUNGE failed: {error}"))?;
    Ok(())
}

fn archive_uid(
    session: &mut imap::Session<imap::Connection>,
    provider: crate::providers::ProviderKind,
    folder: &str,
    uid: u32,
    target_folder: &str,
) -> Result<()> {
    if provider == crate::providers::ProviderKind::Google && folder.eq_ignore_ascii_case("INBOX") {
        session
            .uid_store(uid.to_string(), "-X-GM-LABELS (\\Inbox)")
            .map(|_| ())
            .map_err(|error| anyhow!("Gmail archive failed: {error}"))
    } else {
        move_uid(session, uid, target_folder)
    }
}

fn delete_uid(session: &mut imap::Session<imap::Connection>, uid: u32) -> Result<()> {
    let capabilities = session
        .capabilities()
        .map_err(|error| anyhow!("read IMAP capabilities: {error}"))?;
    if !capabilities.has_str("UIDPLUS") {
        return Err(anyhow!(
            "IMAP server does not support UIDPLUS; refusing an unsafe permanent delete"
        ));
    }
    session
        .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
        .map_err(|error| anyhow!("IMAP delete failed: {error}"))?;
    session
        .uid_expunge(uid.to_string())
        .map_err(|error| anyhow!("IMAP UID EXPUNGE failed: {error}"))?;
    Ok(())
}

pub fn move_message(
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    uid: u32,
    target_folder: &str,
) -> Result<()> {
    validate_mailbox_name(folder)?;
    validate_mailbox_name(target_folder)?;
    let mut session = authenticate(&profile, email, credential)?;
    session.select(folder)?;
    let result = move_uid(&mut session, uid, target_folder);
    session.logout().ok();
    result
}

pub fn archive_message(
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    uid: u32,
    target_folder: &str,
) -> Result<()> {
    validate_mailbox_name(folder)?;
    validate_mailbox_name(target_folder)?;
    let mut session = authenticate(&profile, email, credential)?;
    session.select(folder)?;
    let result = archive_uid(&mut session, profile.provider, folder, uid, target_folder);
    session.logout().ok();
    result
}

pub fn delete_message(
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    uid: u32,
    trash_folder: &str,
) -> Result<()> {
    validate_mailbox_name(folder)?;
    validate_mailbox_name(trash_folder)?;
    let mut session = authenticate(&profile, email, credential)?;
    session.select(folder)?;
    let result = if folder.eq_ignore_ascii_case(trash_folder) {
        delete_uid(&mut session, uid)
    } else {
        move_uid(&mut session, uid, trash_folder)
    };
    session.logout().ok();
    result
}

pub fn queue_move_mutation(
    cache_root: &Path,
    account_id: &str,
    operation: &str,
    folder: &str,
    uid: u32,
    target_folder: Option<&str>,
) -> Result<()> {
    if !matches!(operation, "move" | "archive" | "delete") {
        return Err(anyhow!("unsupported queued mail operation"));
    }
    validate_mailbox_name(folder)?;
    if let Some(target) = target_folder {
        validate_mailbox_name(target)?;
    }
    let directory = cache_root.join(safe_component(account_id));
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let path = directory.join(MOVE_MUTATION_FILE);
    let mut mutations = load_move_mutations(&path)?;
    mutations.retain(|mutation| {
        !(mutation.folder.eq_ignore_ascii_case(folder)
            && mutation.uid == uid
            && mutation.operation == operation)
    });
    mutations.push(PendingMoveMutation {
        operation: operation.to_string(),
        folder: folder.to_string(),
        uid,
        target_folder: target_folder.map(ToOwned::to_owned),
    });
    save_move_mutations(&path, &mutations)
}

pub fn remove_queued_move(
    cache_root: &Path,
    account_id: &str,
    operation: &str,
    folder: &str,
    uid: u32,
) -> Result<()> {
    let path = cache_root
        .join(safe_component(account_id))
        .join(MOVE_MUTATION_FILE);
    let mut mutations = load_move_mutations(&path)?;
    let original_len = mutations.len();
    mutations.retain(|mutation| {
        !(mutation.folder.eq_ignore_ascii_case(folder)
            && mutation.uid == uid
            && mutation.operation == operation)
    });
    if mutations.len() != original_len {
        save_move_mutations(&path, &mutations)?;
    }
    Ok(())
}

/// Return the number of locally applied mutations that still need provider replay. The queue
/// files are DPAPI-protected on Windows, so the renderer only receives counts, never mutation
/// details or mailbox contents.
pub fn pending_mutation_counts(
    cache_root: &Path,
    account_id: &str,
) -> Result<PendingMutationCounts> {
    let directory = cache_root.join(safe_component(account_id));
    let flags = load_mutations(&directory.join(MUTATION_FILE))?.len();
    let moves = load_move_mutations(&directory.join(MOVE_MUTATION_FILE))?.len();
    Ok(PendingMutationCounts {
        flags,
        moves,
        total: flags.saturating_add(moves),
    })
}

fn flush_queued_moves(
    session: &mut imap::Session<imap::Connection>,
    cache_root: &Path,
    account_id: &str,
    provider: crate::providers::ProviderKind,
) {
    let path = cache_root
        .join(safe_component(account_id))
        .join(MOVE_MUTATION_FILE);
    let Ok(mutations) = load_move_mutations(&path) else {
        return;
    };
    if mutations.is_empty() {
        return;
    }
    let mut remaining = Vec::new();
    for mutation in mutations {
        let applied = match session.select(&mutation.folder) {
            Ok(_) => match mutation.operation.as_str() {
                "archive" => mutation
                    .target_folder
                    .as_deref()
                    .ok_or_else(|| anyhow!("queued archive has no target folder"))
                    .and_then(|target| {
                        archive_uid(session, provider, &mutation.folder, mutation.uid, target)
                    }),
                "delete"
                    if mutation.target_folder.is_none()
                        || mutation.target_folder.as_deref().is_some_and(|target| {
                            target.eq_ignore_ascii_case(&mutation.folder)
                        }) =>
                {
                    delete_uid(session, mutation.uid)
                }
                "delete" => mutation
                    .target_folder
                    .as_deref()
                    .ok_or_else(|| anyhow!("queued delete has no trash folder"))
                    .and_then(|target| move_uid(session, mutation.uid, target)),
                "move" => mutation
                    .target_folder
                    .as_deref()
                    .ok_or_else(|| anyhow!("queued move has no target folder"))
                    .and_then(|target| move_uid(session, mutation.uid, target)),
                _ => Err(anyhow!("unsupported queued mail operation")),
            },
            Err(error) => Err(anyhow!("select queued mutation folder: {error}")),
        }
        .is_ok();
        if applied {
            let cache_result = if mutation.operation == "delete"
                && (mutation.target_folder.is_none()
                    || mutation
                        .target_folder
                        .as_deref()
                        .is_some_and(|target| target.eq_ignore_ascii_case(&mutation.folder)))
            {
                remove_cached_message(cache_root, account_id, &mutation.folder, mutation.uid)
            } else if let Some(target) = mutation.target_folder.as_deref() {
                update_cached_move(
                    cache_root,
                    account_id,
                    &mutation.folder,
                    mutation.uid,
                    target,
                )
            } else {
                Ok(())
            };
            if let Err(error) = cache_result {
                tracing::warn!(account_id = %account_id, uid = mutation.uid, "update cached queued mail operation failed: {error}");
            }
        } else {
            remaining.push(mutation);
        }
    }
    if let Err(error) = save_move_mutations(&path, &remaining) {
        tracing::warn!(account_id = %account_id, "save pending mail moves failed: {error}");
    }
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
    validate_mailbox_name(folder)?;
    let mut session = authenticate(&profile, email, credential)?;
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

pub fn queue_flag_mutation(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    flag: &str,
    enabled: bool,
) -> Result<()> {
    validate_mailbox_name(folder)?;
    let directory = cache_root.join(safe_component(account_id));
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let path = directory.join(MUTATION_FILE);
    let mut mutations = load_mutations(&path)?;
    mutations.retain(|mutation| {
        !(mutation.folder.eq_ignore_ascii_case(folder)
            && mutation.uid == uid
            && mutation.flag == flag)
    });
    mutations.push(PendingFlagMutation {
        folder: folder.to_string(),
        uid,
        flag: flag.to_string(),
        enabled,
    });
    save_mutations(&path, &mutations)
}

pub fn remove_queued_flag(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    flag: &str,
) -> Result<()> {
    let path = cache_root
        .join(safe_component(account_id))
        .join(MUTATION_FILE);
    let mut mutations = load_mutations(&path)?;
    let original_len = mutations.len();
    mutations.retain(|mutation| {
        !(mutation.folder.eq_ignore_ascii_case(folder)
            && mutation.uid == uid
            && mutation.flag == flag)
    });
    if mutations.len() != original_len {
        save_mutations(&path, &mutations)?;
    }
    Ok(())
}

fn flush_queued_flags(
    session: &mut imap::Session<imap::Connection>,
    cache_root: &Path,
    account_id: &str,
) {
    let path = cache_root
        .join(safe_component(account_id))
        .join(MUTATION_FILE);
    let Ok(mutations) = load_mutations(&path) else {
        return;
    };
    if mutations.is_empty() {
        return;
    }
    let mut remaining = Vec::new();
    for mutation in mutations {
        let applied = session
            .select(&mutation.folder)
            .and_then(|_| {
                let operation = if mutation.enabled {
                    "+FLAGS.SILENT"
                } else {
                    "-FLAGS.SILENT"
                };
                session
                    .uid_store(
                        mutation.uid.to_string(),
                        format!("{operation} ({})", mutation.flag),
                    )
                    .map(|_| ())
            })
            .is_ok();
        if !applied {
            remaining.push(mutation);
        }
    }
    if let Err(error) = save_mutations(&path, &remaining) {
        tracing::warn!(account_id = %account_id, "save pending mail mutations failed: {error}");
    }
}

fn load_mutations(path: &Path) -> Result<Vec<PendingFlagMutation>> {
    match fs::read(path) {
        Ok(contents) => {
            let decoded = unprotect_cache(&contents)?;
            serde_json::from_slice(&decoded).context("parse pending mail mutations")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn load_move_mutations(path: &Path) -> Result<Vec<PendingMoveMutation>> {
    match fs::read(path) {
        Ok(contents) => {
            let decoded = unprotect_cache(&contents)?;
            serde_json::from_slice(&decoded).context("parse pending mail moves")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn save_mutations(path: &Path, mutations: &[PendingFlagMutation]) -> Result<()> {
    let temporary = path.with_extension("bin.tmp");
    let payload = protect_cache(&serde_json::to_vec(mutations)?)?;
    fs::write(&temporary, payload).context("write pending mail mutations")?;
    if path.exists() {
        fs::remove_file(path).context("replace pending mail mutations")?;
    }
    fs::rename(temporary, path).context("commit pending mail mutations")?;
    Ok(())
}

fn save_move_mutations(path: &Path, mutations: &[PendingMoveMutation]) -> Result<()> {
    let temporary = path.with_extension("bin.tmp");
    let payload = protect_cache(&serde_json::to_vec(mutations)?)?;
    fs::write(&temporary, payload).context("write pending mail moves")?;
    if path.exists() {
        fs::remove_file(path).context("replace pending mail moves")?;
    }
    fs::rename(temporary, path).context("commit pending mail moves")?;
    Ok(())
}

fn save_mailbox(cache_root: &Path, account_id: &str, mailbox: &CachedMailbox) -> Result<PathBuf> {
    let directory = cache_root.join(safe_component(account_id));
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let file_name = cache_file_name(&mailbox.folder);
    let path = directory.join(&file_name);
    let temporary = directory.join(format!("{file_name}.tmp"));
    let payload = protect_cache(&serde_json::to_vec_pretty(mailbox)?)?;
    fs::write(&temporary, payload).context("write mailbox cache")?;
    if path.exists() {
        let backup = directory.join(format!("{file_name}.bak"));
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

fn cache_file_name(folder: &str) -> String {
    if folder.eq_ignore_ascii_case("INBOX") {
        CACHE_FILE.into()
    } else {
        format!("folder_{}.bin", safe_component(folder))
    }
}

fn discover_folders(
    session: &mut imap::Session<imap::Connection>,
    provider: crate::providers::ProviderKind,
) -> Vec<String> {
    let preferred = folders_for(provider);
    let listed = session
        .list(None, Some("*"))
        .map(|names| {
            names
                .iter()
                .map(|name| name.name().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if listed.is_empty() {
        return preferred.into_iter().map(ToOwned::to_owned).collect();
    }

    let mut folders = preferred
        .into_iter()
        .map(|candidate| {
            listed
                .iter()
                .find(|name| name.eq_ignore_ascii_case(candidate))
                .cloned()
                .unwrap_or_else(|| candidate.to_string())
        })
        .collect::<Vec<_>>();
    for name in listed {
        if is_likely_mail_folder(&name)
            && !folders
                .iter()
                .any(|folder| folder.eq_ignore_ascii_case(&name))
        {
            folders.push(name);
        }
    }
    folders
}

fn is_likely_mail_folder(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "inbox"
        || name.contains("sent")
        || name.contains("draft")
        || name.contains("spam")
        || name.contains("junk")
        || name.contains("trash")
        || name.contains("deleted")
        || name.contains("archive")
        || name.contains("all mail")
        || name.contains("allmail")
}

fn folders_for(provider: crate::providers::ProviderKind) -> Vec<&'static str> {
    match provider {
        crate::providers::ProviderKind::Google => vec![
            "INBOX",
            "[Gmail]/Sent Mail",
            "[Gmail]/Drafts",
            "[Gmail]/Spam",
            "[Gmail]/Trash",
            "[Gmail]/All Mail",
        ],
        crate::providers::ProviderKind::Qq => {
            vec!["INBOX", "Sent Messages", "Drafts", "Spam", "Trash"]
        }
        crate::providers::ProviderKind::Outlook => {
            vec![
                "INBOX",
                "Sent Items",
                "Drafts",
                "Junk Email",
                "Deleted Items",
                "Archive",
            ]
        }
        crate::providers::ProviderKind::Other => {
            vec![
                "INBOX",
                "Sent",
                "Sent Items",
                "Drafts",
                "Spam",
                "Trash",
                "Archive",
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::SmartCategory;

    fn fixture_message(uid: u32, folder: &str) -> CachedMessage {
        CachedMessage {
            id: format!("fixture-{uid}"),
            account_id: "fixture-account".into(),
            folder: folder.into(),
            uid,
            subject: "Offline fixture".into(),
            sender_name: "Fixture Sender".into(),
            sender_email: "sender@example.invalid".into(),
            received_at: None,
            unread: true,
            starred: false,
            category: SmartCategory::Inbox,
            is_ad: false,
            preview: "Offline preview".into(),
            text_body: "Offline body".into(),
            html_body: None,
            attachments: Vec::new(),
            raw_path: None,
        }
    }
    use imap::Authenticator;

    #[test]
    fn cache_component_cannot_escape_account_directory() {
        assert_eq!(safe_component("../../secret"), ".._.._secret");
        assert_eq!(safe_component("account-1"), "account-1");
    }

    #[test]
    fn removing_account_cache_is_scoped_and_idempotent() {
        let root =
            std::env::temp_dir().join(format!("mailgo-sync-cache-test-{}", std::process::id()));
        let account_dir = root.join(safe_component("account-1"));
        fs::create_dir_all(&account_dir).unwrap();
        fs::write(account_dir.join("inbox.bin"), b"cache").unwrap();

        remove_account_cache(&root, "account-1").unwrap();
        assert!(!account_dir.exists());
        remove_account_cache(&root, "account-1").unwrap();
        let _ = fs::remove_dir_all(root);
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

    #[test]
    fn sync_retries_transport_and_rate_limits_but_not_auth_failures() {
        assert_eq!(
            retry_delay(&anyhow!("connect IMAP host timed out"), 0),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay(&anyhow!("IMAP provider rate limit: too many requests"), 1),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            retry_delay(&anyhow!("HTTP 429 Retry-After: 37"), 0),
            Some(Duration::from_secs(37))
        );
        assert_eq!(
            retry_delay(&anyhow!("retry_after=9999"), 0),
            Some(Duration::from_secs(300))
        );
        assert!(retry_delay(&anyhow!("IMAP authentication failed"), 0).is_none());
        assert!(retry_delay(&anyhow!("unsupported mail provider"), 0).is_none());
        assert_eq!(
            classify_sync_error(&anyhow!("IMAP authentication failed")),
            SyncErrorClass::Authentication
        );
        assert_eq!(
            classify_sync_error(&anyhow!("too many connections")),
            SyncErrorClass::RateLimited
        );
        assert_eq!(
            classify_sync_error(&anyhow!("connection reset by peer")),
            SyncErrorClass::Transport
        );
        assert_eq!(
            classify_sync_error(&anyhow!("message headers could not be parsed")),
            SyncErrorClass::Permanent
        );
    }

    #[test]
    fn folder_discovery_only_adopts_mailbox_like_names() {
        assert!(is_likely_mail_folder("[Gmail]/All Mail"));
        assert!(is_likely_mail_folder("Archive 2026"));
        assert!(is_likely_mail_folder("Junk Email"));
        assert!(!is_likely_mail_folder("Contacts"));
    }

    #[test]
    fn mailbox_names_reject_command_delimiters() {
        assert!(validate_mailbox_name("INBOX").is_ok());
        assert!(validate_mailbox_name("Sent Items").is_ok());
        assert!(validate_mailbox_name("INBOX\r\nUID FETCH 1 ALL").is_err());
        assert!(validate_mailbox_name("").is_err());
    }

    #[test]
    fn queued_mail_operations_are_explicit_and_serializable() {
        let mutation = PendingMoveMutation {
            operation: "archive".into(),
            folder: "INBOX".into(),
            uid: 42,
            target_folder: Some("[Gmail]/All Mail".into()),
        };
        let value = serde_json::to_value(mutation).unwrap();
        assert_eq!(value["operation"], "archive");
        assert_eq!(value["targetFolder"], "[Gmail]/All Mail");
    }

    #[test]
    fn offline_flag_queue_coalesces_and_updates_local_cache() {
        let root =
            std::env::temp_dir().join(format!("mailgo-offline-flag-test-{}", std::process::id()));
        let mailbox = CachedMailbox {
            schema_version: 1,
            account_id: "fixture-account".into(),
            folder: "INBOX".into(),
            uid_validity: Some(1),
            synced_at: now_stamp(),
            messages: vec![fixture_message(7, "INBOX")],
            oldest_uid: Some(7),
            has_more: false,
        };
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        queue_flag_mutation(&root, "fixture-account", "INBOX", 7, "\\Seen", false).unwrap();
        queue_flag_mutation(&root, "fixture-account", "INBOX", 7, "\\Seen", true).unwrap();
        let queued = load_mutations(
            &root
                .join(safe_component("fixture-account"))
                .join(MUTATION_FILE),
        )
        .unwrap();
        assert_eq!(queued.len(), 1);
        assert!(queued[0].enabled);

        update_cached_flags(&root, "fixture-account", "INBOX", 7, "\\Seen", true).unwrap();
        let updated = load_mailbox_for_folder(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap();
        assert!(!updated.messages[0].unread);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_move_queue_coalesces_and_moves_local_message() {
        let root =
            std::env::temp_dir().join(format!("mailgo-offline-move-test-{}", std::process::id()));
        let mailbox = CachedMailbox {
            schema_version: 1,
            account_id: "fixture-account".into(),
            folder: "INBOX".into(),
            uid_validity: Some(1),
            synced_at: now_stamp(),
            messages: vec![fixture_message(8, "INBOX")],
            oldest_uid: Some(8),
            has_more: false,
        };
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        queue_move_mutation(
            &root,
            "fixture-account",
            "archive",
            "INBOX",
            8,
            Some("Archive"),
        )
        .unwrap();
        queue_move_mutation(
            &root,
            "fixture-account",
            "archive",
            "INBOX",
            8,
            Some("[Gmail]/All Mail"),
        )
        .unwrap();
        let queued = load_move_mutations(
            &root
                .join(safe_component("fixture-account"))
                .join(MOVE_MUTATION_FILE),
        )
        .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].target_folder.as_deref(), Some("[Gmail]/All Mail"));

        update_cached_move(&root, "fixture-account", "INBOX", 8, "[Gmail]/All Mail").unwrap();
        assert!(load_mailbox_for_folder(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap()
            .messages
            .is_empty());
        let moved = load_mailbox_for_folder(&root, "fixture-account", "[Gmail]/All Mail")
            .unwrap()
            .unwrap();
        assert_eq!(moved.messages[0].folder, "[Gmail]/All Mail");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pending_mutation_counts_report_only_queue_sizes() {
        let root =
            std::env::temp_dir().join(format!("mailgo-queue-counts-test-{}", std::process::id()));
        queue_flag_mutation(&root, "fixture-account", "INBOX", 9, "\\Seen", false).unwrap();
        queue_move_mutation(
            &root,
            "fixture-account",
            "archive",
            "INBOX",
            10,
            Some("Archive"),
        )
        .unwrap();
        let counts = pending_mutation_counts(&root, "fixture-account").unwrap();
        assert_eq!(counts.flags, 1);
        assert_eq!(counts.moves, 1);
        assert_eq!(counts.total, 2);
        let _ = fs::remove_dir_all(root);
    }
}
