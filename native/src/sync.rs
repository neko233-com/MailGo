use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use imap::types::Flag;
use native_tls::{TlsConnector, TlsStream};
use serde::{Deserialize, Serialize};

use crate::mail::{parse_full, parse_header, CachedMailbox, CachedMessage};
use crate::providers::{Authentication, ProviderProfile, TransportSecurity};

const HEADER_FETCH_QUERY: &str = "UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]";
const FULL_FETCH_QUERY: &str = "UID FLAGS RFC822";
const MAX_HEADER_MESSAGES: usize = 100;
const MAX_DISCOVERED_FOLDERS: usize = 64;
const MAX_UIDS_PER_QUERY: usize = 100_000;
const MAX_CACHED_MESSAGES_PER_FOLDER: usize = 5_000;
const MAX_CACHE_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 256 * 1024;
const MAX_HEADER_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_SEARCH_QUERY_BYTES: usize = 256;
const MAX_SEARCH_RESULTS_PER_FOLDER: usize = 40;
const MAX_SEARCH_RESULTS_PER_ACCOUNT: usize = 240;
const MAX_MUTATIONS: usize = 1_000;
const MAX_MUTATION_FILE_BYTES: usize = 2 * 1024 * 1024;
const IMAP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const IMAP_IO_TIMEOUT: Duration = Duration::from_secs(60);
const CACHE_FILE: &str = "inbox.bin";
const MUTATION_FILE: &str = "mutations.bin";
const MOVE_MUTATION_FILE: &str = "moves.bin";

static CACHE_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn cache_write_guard() -> std::sync::MutexGuard<'static, ()> {
    CACHE_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("MailGo cache write lock poisoned")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingFlagMutation {
    folder: String,
    uid: u32,
    flag: String,
    enabled: bool,
    #[serde(default)]
    uid_validity: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingMoveMutation {
    operation: String,
    folder: String,
    uid: u32,
    #[serde(default)]
    target_folder: Option<String>,
    #[serde(default)]
    uid_validity: Option<u32>,
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
pub struct SearchResult {
    pub messages: Vec<CachedMessage>,
    pub truncated: bool,
    pub folders_searched: usize,
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
    let tcp = connect_socket(&profile.imap.host, profile.imap.port)?;
    match profile.imap.security {
        TransportSecurity::Tls => {
            let tls = tls_connect(&profile.imap.host, tcp)?;
            let mut client = imap::Client::new(Box::new(tls) as imap::Connection);
            client
                .read_greeting()
                .with_context(|| format!("read IMAP greeting from {}", profile.imap.host))?;
            Ok(client)
        }
        TransportSecurity::StartTls => {
            let mut client = imap::Client::new(tcp);
            client
                .read_greeting()
                .with_context(|| format!("read IMAP greeting from {}", profile.imap.host))?;
            let tcp = client
                .into_inner()
                .context("take IMAP socket before TLS handshake")?;
            let mut tcp = tcp;
            start_tls(&mut tcp)
                .with_context(|| format!("start TLS on IMAP host {}", profile.imap.host))?;
            let tls = tls_connect(&profile.imap.host, tcp)?;
            // The IMAP greeting was already consumed before STARTTLS. LOGIN/AUTHENTICATE is the
            // next legal command after the TLS negotiation, so do not wait for a second greeting.
            Ok(imap::Client::new(Box::new(tls) as imap::Connection))
        }
    }
}

fn connect_socket(host: &str, port: u16) -> Result<TcpStream> {
    let addresses = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve IMAP host {host}"))?;
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, IMAP_CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(IMAP_IO_TIMEOUT))
                    .context("set IMAP read timeout")?;
                stream
                    .set_write_timeout(Some(IMAP_IO_TIMEOUT))
                    .context("set IMAP write timeout")?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow!(
        "could not connect to IMAP host {host}:{port}: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no socket address was returned".to_string())
    ))
}

fn start_tls(tcp: &mut TcpStream) -> Result<()> {
    const STARTTLS_TAG: &[u8] = b"MAILGO1";
    tcp.write_all(STARTTLS_TAG)
        .and_then(|_| tcp.write_all(b" STARTTLS\r\n"))
        .and_then(|_| tcp.flush())
        .context("write IMAP STARTTLS command")?;

    let cloned = tcp.try_clone().context("clone IMAP socket for response")?;
    let mut reader = BufReader::new(cloned);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .context("read IMAP STARTTLS response")?;
        if read == 0 {
            return Err(anyhow!("IMAP server closed the connection before STARTTLS"));
        }
        if line.len() > 64 * 1024 {
            return Err(anyhow!("IMAP STARTTLS response is too large"));
        }
        if !line.starts_with(STARTTLS_TAG) {
            continue;
        }
        let status = line
            .split(|byte| byte.is_ascii_whitespace())
            .nth(1)
            .and_then(|value| std::str::from_utf8(value).ok())
            .unwrap_or_default();
        if status.eq_ignore_ascii_case("OK") {
            return Ok(());
        }
        return Err(anyhow!("IMAP STARTTLS command was rejected"));
    }
}

fn tls_connect(host: &str, tcp: TcpStream) -> Result<TlsStream<TcpStream>> {
    let connector = TlsConnector::builder()
        .build()
        .context("build IMAP TLS connector")?;
    let mut tls = connector
        .connect(host, tcp)
        .map_err(|error| anyhow!("IMAP TLS handshake failed: {error}"))?;
    tls.get_mut()
        .set_read_timeout(Some(IMAP_IO_TIMEOUT))
        .context("set IMAP TLS read timeout")?;
    tls.get_mut()
        .set_write_timeout(Some(IMAP_IO_TIMEOUT))
        .context("set IMAP TLS write timeout")?;
    Ok(tls)
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
                    Err(error) => {
                        crate::record_account_sync_failure(
                            &shared,
                            &account.id,
                            needs_reauthorization(&error),
                        );
                        tracing::warn!(
                            account_id = %account.id,
                            "background credential load failed: {error}"
                        );
                        continue;
                    }
                };
                if let Err(error) = crate::outbox::flush_due(
                    &cache_root,
                    &account.id,
                    profile.clone(),
                    &account.email,
                    &credential,
                ) {
                    tracing::warn!(
                        account_id = %account.id,
                        "background outbox flush failed: {error}"
                    );
                }
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
                    Err(error) => {
                        crate::record_account_sync_failure(
                            &shared,
                            &account.id,
                            needs_reauthorization(&error),
                        );
                        tracing::warn!(
                            account_id = %account.id,
                            "background sync failed: {error}"
                        );
                    }
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

/// Search the provider's full mailbox set without requiring the renderer to preload every
/// message. Results are header-only, bounded, and immediately merged into the encrypted local
/// folder cache so subsequent offline actions retain UIDVALIDITY context.
pub fn search_account(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    query: &str,
    limit: usize,
    cache_root: &Path,
) -> Result<SearchResult> {
    if credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before searching"));
    }
    let query = build_search_query(query)?;
    let limit = limit.clamp(1, MAX_SEARCH_RESULTS_PER_ACCOUNT);
    let mut session = authenticate(&profile, email, credential)?;
    let folders = discover_folders(&mut session, profile.provider);
    let mut messages = Vec::new();
    let mut truncated = false;
    let mut folders_searched = 0usize;

    for folder in folders {
        if messages.len() >= limit {
            truncated = true;
            break;
        }
        let mailbox = match session.select(&folder) {
            Ok(mailbox) => mailbox,
            Err(error) => {
                tracing::debug!(account_id = %account_id, folder = %folder, "search skipped unavailable folder: {error}");
                continue;
            }
        };
        folders_searched += 1;
        let mut uids = session
            .uid_search(&query)
            .with_context(|| format!("search {folder}"))?
            .into_iter()
            .collect::<Vec<_>>();
        uids.sort_unstable_by(|left, right| right.cmp(left));
        let remaining = limit.saturating_sub(messages.len());
        let folder_limit = remaining.min(MAX_SEARCH_RESULTS_PER_FOLDER);
        if uids.len() > folder_limit {
            truncated = true;
        }
        let selected_uids = uids.into_iter().take(folder_limit).collect::<Vec<_>>();
        let uid_set = selected_uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        if uid_set.is_empty() {
            continue;
        }
        let fetched = session
            .uid_fetch(uid_set, HEADER_FETCH_QUERY)
            .with_context(|| format!("fetch search results in {folder}"))?;
        let mut header_bytes = 0usize;
        let mut folder_messages = Vec::new();
        for item in fetched.iter() {
            if messages.len() >= limit {
                truncated = true;
                break;
            }
            let Some(uid) = item.uid else { continue };
            let Some(header) = item.header() else {
                continue;
            };
            if header.len() > MAX_HEADER_BYTES {
                continue;
            }
            header_bytes = header_bytes.saturating_add(header.len());
            if header_bytes > MAX_HEADER_TOTAL_BYTES {
                truncated = true;
                break;
            }
            let unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
            let starred = item
                .flags()
                .iter()
                .any(|flag| matches!(flag, Flag::Flagged));
            let Ok(message) = parse_header(account_id, &folder, uid, unread, starred, header)
            else {
                continue;
            };
            folder_messages.push(message);
        }
        save_search_messages(
            cache_root,
            account_id,
            mailbox.uid_validity,
            &folder_messages,
        )?;
        messages.extend(folder_messages);
    }
    session.logout().ok();
    messages.sort_by(|left, right| right.received_at.cmp(&left.received_at));
    Ok(SearchResult {
        messages,
        truncated,
        folders_searched,
    })
}

fn build_search_query(value: &str) -> Result<String> {
    let normalized = value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    if normalized.is_empty() {
        return Err(anyhow!("search query is empty"));
    }
    if normalized.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(anyhow!("search query is too long"));
    }
    let escaped = normalized.replace('\\', "\\\\").replace('"', "\\\"");
    // IMAP SEARCH keys are nested left-to-right: (FROM OR TO) OR (SUBJECT OR TEXT).
    Ok(format!(
        "OR OR FROM \"{escaped}\" TO \"{escaped}\" OR SUBJECT \"{escaped}\" TEXT \"{escaped}\""
    ))
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
    if all_uids.len() > MAX_UIDS_PER_QUERY {
        all_uids.drain(..all_uids.len() - MAX_UIDS_PER_QUERY);
    }
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
        let mut header_bytes = 0usize;
        for item in fetched.iter() {
            let Some(uid) = item.uid else { continue };
            let Some(header) = item.header() else {
                continue;
            };
            if header.len() > MAX_HEADER_BYTES {
                continue;
            }
            header_bytes = header_bytes.saturating_add(header.len());
            if header_bytes > MAX_HEADER_TOTAL_BYTES {
                break;
            }
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
#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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
    if uids.len() > MAX_UIDS_PER_QUERY {
        uids.drain(..uids.len() - MAX_UIDS_PER_QUERY);
    }
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
        let mut header_bytes = 0usize;
        for item in fetched.iter() {
            let Some(uid) = item.uid else { continue };
            let Some(header) = item.header() else {
                continue;
            };
            if header.len() > MAX_HEADER_BYTES {
                continue;
            }
            header_bytes = header_bytes.saturating_add(header.len());
            if header_bytes > MAX_HEADER_TOTAL_BYTES {
                break;
            }
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

pub fn needs_reauthorization(error: &anyhow::Error) -> bool {
    classify_sync_error(error) == SyncErrorClass::Authentication
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
    validate_mailbox_name(folder)?;
    let directory = cache_root.join(safe_component(account_id));
    let encrypted_path = directory.join(cache_file_name(folder));
    let mut candidates = vec![
        (encrypted_path.clone(), true),
        (encrypted_path.with_extension("bin.bak"), true),
    ];
    if folder.eq_ignore_ascii_case("INBOX") {
        let legacy_path = directory.join("inbox.json");
        candidates.push((legacy_path.clone(), false));
        candidates.push((legacy_path.with_extension("json.bak"), false));
    }
    let mut first_error = None;
    for (path, encrypted) in candidates {
        match load_mailbox_file(&path, account_id, folder, encrypted) {
            Ok(mailbox) => {
                if first_error.is_some() {
                    tracing::warn!(
                        account_id = %account_id,
                        folder = %folder,
                        "mailbox cache primary was invalid; recovered from backup"
                    );
                }
                return Ok(Some(mailbox));
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(None)
}

fn load_mailbox_file(
    path: &Path,
    account_id: &str,
    folder: &str,
    encrypted: bool,
) -> Result<CachedMailbox> {
    let contents = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if contents.len() > MAX_CACHE_FILE_BYTES {
        return Err(anyhow!("mailbox cache is too large"));
    }
    let decoded = if encrypted {
        unprotect_cache(&contents).with_context(|| format!("decrypt {}", path.display()))?
    } else {
        contents
    };
    if decoded.len() > MAX_CACHE_FILE_BYTES {
        return Err(anyhow!("mailbox cache is too large"));
    }
    let mailbox: CachedMailbox =
        serde_json::from_slice(&decoded).with_context(|| format!("parse {}", path.display()))?;
    if mailbox.account_id != account_id || !mailbox.folder.eq_ignore_ascii_case(folder) {
        return Err(anyhow!("cache identity mismatch in {}", path.display()));
    }
    if mailbox.messages.len() > MAX_CACHED_MESSAGES_PER_FOLDER {
        return Err(anyhow!("mailbox cache contains too many messages"));
    }
    let mut mailbox = mailbox;
    for message in &mut mailbox.messages {
        crate::mail::bound_cached_message(message);
    }
    Ok(mailbox)
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
    let _write_guard = cache_write_guard();
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
    save_mailbox_unlocked(cache_root, account_id, &mailbox).map(|_| ())
}

fn save_search_messages(
    cache_root: &Path,
    account_id: &str,
    uid_validity: Option<u32>,
    messages: &[CachedMessage],
) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let _write_guard = cache_write_guard();
    let folder = messages[0].folder.as_str();
    let mut mailbox = load_mailbox_for_folder(cache_root, account_id, folder)?
        .unwrap_or_else(|| CachedMailbox::empty(account_id, folder));
    if mailbox.uid_validity.is_some() && mailbox.uid_validity != uid_validity {
        // A search can discover a folder before the regular sync scheduler does. Never merge
        // results from a new UID namespace into an old cache.
        mailbox.messages.clear();
        mailbox.oldest_uid = None;
        mailbox.has_more = false;
    }
    mailbox.uid_validity = uid_validity;
    for message in messages {
        mailbox
            .messages
            .retain(|cached| cached.uid != message.uid || cached.folder != message.folder);
        mailbox.messages.push(message.clone());
    }
    mailbox
        .messages
        .sort_by_key(|cached| std::cmp::Reverse(cached.uid));
    mailbox.messages.truncate(MAX_CACHED_MESSAGES_PER_FOLDER);
    mailbox.oldest_uid = mailbox.messages.iter().map(|cached| cached.uid).min();
    mailbox.synced_at = now_stamp();
    save_mailbox_unlocked(cache_root, account_id, &mailbox).map(|_| ())
}

pub fn update_cached_flags(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    flag: &str,
    enabled: bool,
) -> Result<()> {
    let _write_guard = cache_write_guard();
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
        save_mailbox_unlocked(cache_root, account_id, &mailbox)?;
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
    let _write_guard = cache_write_guard();
    update_cached_move_unlocked(cache_root, account_id, folder, uid, target_folder)
}

fn update_cached_move_unlocked(
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
    save_mailbox_unlocked(cache_root, account_id, &target)?;
    save_mailbox_unlocked(cache_root, account_id, &source)?;
    Ok(())
}

pub fn remove_cached_message(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<()> {
    let _write_guard = cache_write_guard();
    remove_cached_message_unlocked(cache_root, account_id, folder, uid)
}

fn remove_cached_message_unlocked(
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
    save_mailbox_unlocked(cache_root, account_id, &mailbox).map(|_| ())
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
    if folder.eq_ignore_ascii_case(trash_folder) && !is_trash_folder(profile.provider, folder) {
        return Err(anyhow!(
            "permanent delete is only allowed from the provider trash folder"
        ));
    }
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

pub fn is_trash_folder(provider: crate::providers::ProviderKind, folder: &str) -> bool {
    let expected = match provider {
        crate::providers::ProviderKind::Google => "[Gmail]/Trash",
        crate::providers::ProviderKind::Qq => "Trash",
        crate::providers::ProviderKind::Outlook => "Deleted Items",
        crate::providers::ProviderKind::Other => "Trash",
    };
    folder.eq_ignore_ascii_case(expected)
}

pub fn queue_move_mutation(
    cache_root: &Path,
    account_id: &str,
    operation: &str,
    folder: &str,
    uid: u32,
    target_folder: Option<&str>,
) -> Result<()> {
    let _write_guard = cache_write_guard();
    if !matches!(operation, "move" | "archive" | "delete") {
        return Err(anyhow!("unsupported queued mail operation"));
    }
    validate_mailbox_name(folder)?;
    if let Some(target) = target_folder {
        validate_mailbox_name(target)?;
    }
    let uid_validity = load_mailbox_for_folder(cache_root, account_id, folder)?
        .and_then(|mailbox| mailbox.uid_validity);
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
        uid_validity,
    });
    save_move_mutations_unlocked(&path, &mutations)
}

pub fn remove_queued_move(
    cache_root: &Path,
    account_id: &str,
    operation: &str,
    folder: &str,
    uid: u32,
) -> Result<()> {
    let _write_guard = cache_write_guard();
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
        save_move_mutations_unlocked(&path, &mutations)?;
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
    let _write_guard = cache_write_guard();
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
        let selected = match session.select(&mutation.folder) {
            Ok(mailbox) => mailbox,
            Err(error) => {
                remaining.push(mutation);
                tracing::debug!(account_id = %account_id, "select queued move folder failed: {error}");
                continue;
            }
        };
        if !uid_validity_matches(mutation.uid_validity, selected.uid_validity) {
            tracing::warn!(account_id = %account_id, uid = mutation.uid, "discarding stale queued mail move after UIDVALIDITY changed");
            continue;
        }
        let permanent_delete = mutation.operation == "delete"
            && (mutation.target_folder.is_none()
                || mutation
                    .target_folder
                    .as_deref()
                    .is_some_and(|target| target.eq_ignore_ascii_case(&mutation.folder)));
        if permanent_delete && !is_trash_folder(provider, &mutation.folder) {
            tracing::warn!(account_id = %account_id, uid = mutation.uid, "discarding unsafe queued permanent delete outside provider trash");
            continue;
        }
        let applied = match mutation.operation.as_str() {
            "archive" => mutation
                .target_folder
                .as_deref()
                .ok_or_else(|| anyhow!("queued archive has no target folder"))
                .and_then(|target| {
                    archive_uid(session, provider, &mutation.folder, mutation.uid, target)
                }),
            "delete" if permanent_delete => delete_uid(session, mutation.uid),
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
                remove_cached_message_unlocked(
                    cache_root,
                    account_id,
                    &mutation.folder,
                    mutation.uid,
                )
            } else if let Some(target) = mutation.target_folder.as_deref() {
                update_cached_move_unlocked(
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
    if let Err(error) = save_move_mutations_unlocked(&path, &remaining) {
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
    let _write_guard = cache_write_guard();
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
        uid_validity: load_mailbox_for_folder(cache_root, account_id, folder)?
            .and_then(|mailbox| mailbox.uid_validity),
    });
    save_mutations_unlocked(&path, &mutations)
}

pub fn remove_queued_flag(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    flag: &str,
) -> Result<()> {
    let _write_guard = cache_write_guard();
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
        save_mutations_unlocked(&path, &mutations)?;
    }
    Ok(())
}

fn flush_queued_flags(
    session: &mut imap::Session<imap::Connection>,
    cache_root: &Path,
    account_id: &str,
) {
    let _write_guard = cache_write_guard();
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
        let selected = match session.select(&mutation.folder) {
            Ok(mailbox) => mailbox,
            Err(error) => {
                remaining.push(mutation);
                tracing::debug!(account_id = %account_id, "select queued flag folder failed: {error}");
                continue;
            }
        };
        if !uid_validity_matches(mutation.uid_validity, selected.uid_validity) {
            tracing::warn!(account_id = %account_id, uid = mutation.uid, "discarding stale queued mail flag after UIDVALIDITY changed");
            continue;
        }
        let applied = {
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
        }
        .is_ok();
        if !applied {
            remaining.push(mutation);
        }
    }
    if let Err(error) = save_mutations_unlocked(&path, &remaining) {
        tracing::warn!(account_id = %account_id, "save pending mail mutations failed: {error}");
    }
}

fn uid_validity_matches(expected: Option<u32>, current: Option<u32>) -> bool {
    expected.is_some_and(|value| current == Some(value))
}

fn load_mutations(path: &Path) -> Result<Vec<PendingFlagMutation>> {
    match fs::read(path) {
        Ok(contents) => {
            if contents.len() > MAX_MUTATION_FILE_BYTES {
                return Err(anyhow!("pending mail flag mutations are too large"));
            }
            let decoded = unprotect_cache(&contents)?;
            if decoded.len() > MAX_MUTATION_FILE_BYTES {
                return Err(anyhow!("pending mail flag mutations are too large"));
            }
            let mutations: Vec<PendingFlagMutation> =
                serde_json::from_slice(&decoded).context("parse pending mail mutations")?;
            if mutations.len() > MAX_MUTATIONS {
                return Err(anyhow!("too many pending mail flag mutations"));
            }
            Ok(mutations)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn load_move_mutations(path: &Path) -> Result<Vec<PendingMoveMutation>> {
    match fs::read(path) {
        Ok(contents) => {
            if contents.len() > MAX_MUTATION_FILE_BYTES {
                return Err(anyhow!("pending mail move mutations are too large"));
            }
            let decoded = unprotect_cache(&contents)?;
            if decoded.len() > MAX_MUTATION_FILE_BYTES {
                return Err(anyhow!("pending mail move mutations are too large"));
            }
            let mutations: Vec<PendingMoveMutation> =
                serde_json::from_slice(&decoded).context("parse pending mail moves")?;
            if mutations.len() > MAX_MUTATIONS {
                return Err(anyhow!("too many pending mail move mutations"));
            }
            Ok(mutations)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn save_mutations_unlocked(path: &Path, mutations: &[PendingFlagMutation]) -> Result<()> {
    if mutations.len() > MAX_MUTATIONS {
        return Err(anyhow!("too many pending mail flag mutations"));
    }
    let temporary = path.with_extension("bin.tmp");
    let serialized = serde_json::to_vec(mutations)?;
    if serialized.len() > MAX_MUTATION_FILE_BYTES {
        return Err(anyhow!("pending mail flag mutations are too large"));
    }
    let payload = protect_cache(&serialized)?;
    if payload.len() > MAX_MUTATION_FILE_BYTES {
        return Err(anyhow!("pending mail flag mutations are too large"));
    }
    fs::write(&temporary, payload).context("write pending mail mutations")?;
    if path.exists() {
        fs::remove_file(path).context("replace pending mail mutations")?;
    }
    fs::rename(temporary, path).context("commit pending mail mutations")?;
    Ok(())
}

fn save_move_mutations_unlocked(path: &Path, mutations: &[PendingMoveMutation]) -> Result<()> {
    if mutations.len() > MAX_MUTATIONS {
        return Err(anyhow!("too many pending mail move mutations"));
    }
    let temporary = path.with_extension("bin.tmp");
    let serialized = serde_json::to_vec(mutations)?;
    if serialized.len() > MAX_MUTATION_FILE_BYTES {
        return Err(anyhow!("pending mail move mutations are too large"));
    }
    let payload = protect_cache(&serialized)?;
    if payload.len() > MAX_MUTATION_FILE_BYTES {
        return Err(anyhow!("pending mail move mutations are too large"));
    }
    fs::write(&temporary, payload).context("write pending mail moves")?;
    if path.exists() {
        fs::remove_file(path).context("replace pending mail moves")?;
    }
    fs::rename(temporary, path).context("commit pending mail moves")?;
    Ok(())
}

fn save_mailbox(cache_root: &Path, account_id: &str, mailbox: &CachedMailbox) -> Result<PathBuf> {
    let _write_guard = cache_write_guard();
    save_mailbox_unlocked(cache_root, account_id, mailbox)
}

fn save_mailbox_unlocked(
    cache_root: &Path,
    account_id: &str,
    mailbox: &CachedMailbox,
) -> Result<PathBuf> {
    let directory = cache_root.join(safe_component(account_id));
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let file_name = cache_file_name(&mailbox.folder);
    let path = directory.join(&file_name);
    let temporary = directory.join(format!("{file_name}.tmp"));
    let mut bounded_mailbox = mailbox.clone();
    if bounded_mailbox.messages.len() > MAX_CACHED_MESSAGES_PER_FOLDER {
        bounded_mailbox
            .messages
            .truncate(MAX_CACHED_MESSAGES_PER_FOLDER);
        bounded_mailbox.has_more = true;
        bounded_mailbox.oldest_uid = bounded_mailbox
            .messages
            .iter()
            .map(|message| message.uid)
            .min();
    }
    let serialized = serde_json::to_vec_pretty(&bounded_mailbox)?;
    if serialized.len() > MAX_CACHE_FILE_BYTES {
        return Err(anyhow!("mailbox cache is too large"));
    }
    let payload = protect_cache(&serialized)?;
    if payload.len() > MAX_CACHE_FILE_BYTES {
        return Err(anyhow!("mailbox cache is too large"));
    }
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
pub(crate) fn protect_cache(payload: &[u8]) -> Result<Vec<u8>> {
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
pub(crate) fn protect_cache(_payload: &[u8]) -> Result<Vec<u8>> {
    Err(anyhow!(
        "MailGo requires Windows data protection for local cache storage"
    ))
}

#[cfg(target_os = "windows")]
pub(crate) fn unprotect_cache(payload: &[u8]) -> Result<Vec<u8>> {
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
pub(crate) fn unprotect_cache(_payload: &[u8]) -> Result<Vec<u8>> {
    Err(anyhow!(
        "MailGo requires Windows data protection for local cache storage"
    ))
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
    } else if safe == "." || safe == ".." {
        "_reserved_".into()
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
                .filter(|name| is_safe_discovered_folder(name))
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
        if !folders
            .iter()
            .any(|folder| folder.eq_ignore_ascii_case(&name))
            && folders.len() < MAX_DISCOVERED_FOLDERS
        {
            folders.push(name);
        }
    }
    folders
}

fn is_safe_discovered_folder(name: &str) -> bool {
    name.trim().len() <= 512
        && !name.trim().is_empty()
        && !name
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
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

    #[test]
    fn imap_socket_applies_connect_and_io_timeouts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fixture socket");
        let port = listener.local_addr().expect("fixture address").port();
        let stream = connect_socket("127.0.0.1", port).expect("connect fixture socket");
        assert_eq!(stream.read_timeout().unwrap(), Some(IMAP_IO_TIMEOUT));
        assert_eq!(stream.write_timeout().unwrap(), Some(IMAP_IO_TIMEOUT));
    }

    fn fixture_message(uid: u32, folder: &str) -> CachedMessage {
        CachedMessage {
            id: format!("fixture-{uid}"),
            account_id: "fixture-account".into(),
            folder: folder.into(),
            uid,
            subject: "Offline fixture".into(),
            sender_name: "Fixture Sender".into(),
            sender_email: "sender@example.invalid".into(),
            to: Vec::new(),
            cc: Vec::new(),
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
        assert_ne!(safe_component("."), ".");
        assert_ne!(safe_component(".."), "..");
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
        assert!(needs_reauthorization(&anyhow!(
            "OAuth access token expired; reauthorization is required"
        )));
        assert!(!needs_reauthorization(&anyhow!("connection reset by peer")));
    }

    #[test]
    fn server_search_query_is_bounded_and_quoted() {
        let query = build_search_query("  release \\\"candidate\\\"  ").unwrap();
        assert!(query.contains("FROM \"release \\\\\\\"candidate\\\\\\\"\""));
        assert!(query.contains("TEXT \"release \\\\\\\"candidate\\\\\\\"\""));
        assert!(!query.contains('\r'));
        assert!(!query.contains('\n'));
        assert!(build_search_query(" ").is_err());
        assert!(build_search_query(&"x".repeat(MAX_SEARCH_QUERY_BYTES + 1)).is_err());
    }

    #[test]
    fn folder_discovery_accepts_custom_mailbox_names_safely() {
        assert!(is_safe_discovered_folder("Projects/2026"));
        assert!(is_safe_discovered_folder("团队收件箱"));
        assert!(!is_safe_discovered_folder("Contacts\r\nUID FETCH 1 ALL"));
        assert!(!is_safe_discovered_folder(""));
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
            uid_validity: Some(7),
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
    fn mailbox_cache_recovers_from_previous_atomic_snapshot() {
        let root =
            std::env::temp_dir().join(format!("mailgo-cache-backup-test-{}", std::process::id()));
        let first = CachedMailbox {
            schema_version: 1,
            account_id: "fixture-account".into(),
            folder: "INBOX".into(),
            uid_validity: Some(1),
            synced_at: now_stamp(),
            messages: vec![fixture_message(1, "INBOX")],
            oldest_uid: Some(1),
            has_more: false,
        };
        let mut second = first.clone();
        second.messages[0].uid = 2;
        save_mailbox(&root, "fixture-account", &first).unwrap();
        let directory = root.join(safe_component("fixture-account"));
        let primary = directory.join(CACHE_FILE);
        let backup = directory.join(format!("{CACHE_FILE}.bak"));
        fs::rename(&primary, &backup).unwrap();
        fs::write(
            &primary,
            protect_cache(&serde_json::to_vec_pretty(&second).unwrap()).unwrap(),
        )
        .unwrap();
        fs::write(&primary, b"corrupt cache").unwrap();

        let recovered = load_mailbox_for_folder(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap();
        assert_eq!(recovered.messages[0].uid, 1);
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
