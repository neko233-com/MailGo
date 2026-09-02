use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use base64::{
    alphabet::IMAP_MUTF7,
    engine::general_purpose::{GeneralPurpose, NO_PAD},
    Engine as _,
};
use imap::extensions::idle::{self, SetReadTimeout, WaitOutcome};
use imap::types::{Flag, UnsolicitedResponse};
use native_tls::{TlsConnector, TlsStream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mail::{
    parse_full_with_attachments, parse_header, CachedMailbox, CachedMessage, CACHE_SCHEMA_VERSION,
};
use crate::providers::{Authentication, ProviderKind, ProviderProfile, TransportSecurity};

// RFC 3501 requires a parenthesized data-item list when FETCH requests more than one item.
// Some servers accept the unparenthesized form, while stricter providers such as QQ return BAD.
const HEADER_FETCH_QUERY: &str = "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER]<0.262145>)";
const FLAGS_FETCH_QUERY: &str = "(UID FLAGS)";
const SIZE_FETCH_QUERY: &str = "(UID RFC822.SIZE)";
const FULL_FETCH_QUERY: &str = "(UID FLAGS BODY.PEEK[]<0.67108865>)";
const MAX_HEADER_MESSAGES: usize = 100;
const MAX_HEADER_FETCH_BATCH: usize = 30;
const MAX_DISCOVERED_FOLDERS: usize = 64;
const MAX_UIDS_PER_QUERY: usize = 100_000;
const MAX_UID_SEARCH_WINDOW: u32 = 100_000;
const MAX_UID_SEARCH_RANGE_WIDTH: u32 = 8_192;
const MAX_CACHED_MESSAGES_PER_FOLDER: usize = 5_000;
const MAX_CACHE_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 256 * 1024;
const MAX_HEADER_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_SEARCH_QUERY_BYTES: usize = 256;
const MAX_SEARCH_RESULTS_PER_FOLDER: usize = 40;
const MAX_SEARCH_RESULTS_PER_ACCOUNT: usize = 240;
const MAX_MUTATIONS: usize = 1_000;
const MAX_MUTATION_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DELTA_HEADER_UIDS: usize = MAX_HEADER_MESSAGES;
const MAX_DELTA_VANISHED_RANGES: usize = MAX_CACHED_MESSAGES_PER_FOLDER;
const IMAP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const IMAP_IO_TIMEOUT: Duration = Duration::from_secs(60);
const STARTTLS_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_STARTTLS_LINE_BYTES: usize = 64 * 1024;
const MAX_STARTTLS_RESPONSE_BYTES: usize = 256 * 1024;
const INITIAL_SYNC_DELAY: Duration = Duration::from_secs(3);
const BACKGROUND_SYNC_INTERVAL: Duration = Duration::from_secs(300);
const OUTBOX_SCHEDULER_IDLE: Duration = Duration::from_secs(300);
const OUTBOX_SCHEDULER_BUSY_RETRY: Duration = Duration::from_secs(2);
pub(crate) const ACCOUNT_SYNC_CONCURRENCY: usize = 3;
const IDLE_STATE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_PAUSED_RECHECK: Duration = Duration::from_secs(5);
const IDLE_UNSUPPORTED_RECHECK: Duration = Duration::from_secs(30 * 60);
const IDLE_SUPERVISOR_INTERVAL: Duration = Duration::from_secs(5);
const IDLE_RETRY_BASE_SECONDS: u64 = 15;
const IDLE_RETRY_MAX_SECONDS: u64 = 300;
const IDLE_SYNC_BUSY_RETRIES: usize = 12;
const BODY_SESSION_IDLE_TTL: Duration = Duration::from_secs(5 * 60);
const CACHE_FILE: &str = "inbox.bin";
const MUTATION_FILE: &str = "mutations.bin";
const MOVE_MUTATION_FILE: &str = "moves.bin";
const CACHE_ENVELOPE_MAGIC: &[u8] = b"MAILGO-CACHE-1\0";
const CACHE_ENCRYPTION_KEY_SERVICE: &str = "com.neko233.mailgo.cache";
const CACHE_ENCRYPTION_KEY_ACCOUNT: &str = "cache-encryption-key";
const CACHE_ENCRYPTION_KEY_BYTES: usize = 32;

static CACHE_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static CACHE_ENCRYPTION_KEY: OnceLock<Mutex<Option<[u8; CACHE_ENCRYPTION_KEY_BYTES]>>> =
    OnceLock::new();
type BodySessionSlot = Arc<Mutex<Option<PooledBodySession>>>;
type BodySessionRegistry = HashMap<String, BodySessionSlot>;

static BODY_SESSIONS: OnceLock<Mutex<BodySessionRegistry>> = OnceLock::new();

pub(crate) fn map_with_concurrency<T, R, F>(items: &[T], concurrency: usize, task: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    assert!(concurrency > 0, "concurrency must be positive");
    if items.is_empty() {
        return Vec::new();
    }

    let cursor = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(items.len()));
    thread::scope(|scope| {
        for _ in 0..concurrency.min(items.len()) {
            scope.spawn(|| loop {
                let index = cursor.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(index) else { return };
                let result = task(item);
                results
                    .lock()
                    .expect("bounded worker result lock poisoned")
                    .push((index, result));
            });
        }
    });
    let mut results = results
        .into_inner()
        .expect("bounded worker result lock poisoned");
    results.sort_unstable_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

struct PooledBodySession {
    credential_fingerprint: [u8; 32],
    selected_folder: Option<String>,
    last_used: Instant,
    session: imap::Session<imap::Connection>,
}

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
    pub folder_labels: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct MailboxSyncDelta {
    new_messages: Vec<CachedMessage>,
    flag_updates: Vec<(u32, bool, bool)>,
    removed_uids: Vec<u32>,
}

impl MailboxSyncDelta {
    fn is_empty(&self) -> bool {
        self.new_messages.is_empty() && self.flag_updates.is_empty() && self.removed_uids.is_empty()
    }
}

#[derive(Debug)]
struct FolderSyncOutcome {
    mailbox: CachedMailbox,
    fetched: usize,
    delta: MailboxSyncDelta,
    force_full_rewrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub messages: Vec<CachedMessage>,
    pub truncated: bool,
    pub folders_searched: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UidSearchOrder {
    Ascending,
    Descending,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct BoundedUidSearch {
    uids: Vec<u32>,
    truncated: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct LatestUidSelection {
    selected_uids: Vec<u32>,
    new_uid_count: Option<usize>,
    rebase_to_latest: bool,
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

/// Capability-driven incremental synchronization. QRESYNC gives us server-side deletion
/// deltas; CONDSTORE gives us changed flags and is paired with a bounded UID existence check.
/// Servers that advertise neither extension continue through the conservative UID path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncrementalMode {
    Condstore,
    Qresync,
}

struct BoundedImapStream<T> {
    inner: T,
}

impl<T> BoundedImapStream<T> {
    fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: Read> Read for BoundedImapStream<T> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buffer)
    }
}

impl<T: Write> Write for BoundedImapStream<T> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<T: SetReadTimeout> SetReadTimeout for BoundedImapStream<T> {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> imap::error::Result<()> {
        self.inner
            .set_read_timeout(timeout.or(Some(IMAP_IO_TIMEOUT)))
    }
}

fn connect(profile: &ProviderProfile) -> Result<imap::Client<imap::Connection>> {
    let tcp = connect_socket(&profile.imap.host, profile.imap.port)?;
    match profile.imap.security {
        TransportSecurity::Tls => {
            let tls = tls_connect(&profile.imap.host, tcp)?;
            let mut client =
                imap::Client::new(Box::new(BoundedImapStream::new(tls)) as imap::Connection);
            client
                .read_greeting()
                .with_context(|| format!("read IMAP greeting from {}", profile.imap.host))?;
            Ok(client)
        }
        TransportSecurity::StartTls => {
            let tcp = start_tls(tcp)
                .with_context(|| format!("start TLS on IMAP host {}", profile.imap.host))?;
            let tls = tls_connect(&profile.imap.host, tcp)?;
            // The IMAP greeting was already consumed before STARTTLS. LOGIN/AUTHENTICATE is the
            // next legal command after the TLS negotiation, so do not wait for a second greeting.
            Ok(imap::Client::new(
                Box::new(BoundedImapStream::new(tls)) as imap::Connection
            ))
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

fn set_starttls_read_deadline(stream: &TcpStream, deadline: Instant) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| anyhow!("IMAP STARTTLS exchange timed out"))?;
    stream
        .set_read_timeout(Some(remaining.min(IMAP_IO_TIMEOUT)))
        .context("set IMAP STARTTLS read deadline")
}

fn read_bounded_starttls_line<R, F>(
    reader: &mut R,
    line: &mut Vec<u8>,
    mut before_read: F,
) -> Result<usize>
where
    R: BufRead,
    F: FnMut(&mut R) -> Result<()>,
{
    line.clear();
    loop {
        before_read(reader)?;
        let available = reader.fill_buf().context("read IMAP plaintext response")?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_STARTTLS_LINE_BYTES {
            return Err(anyhow!("IMAP STARTTLS response line is too large"));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(line.len());
        }
    }
}

fn start_tls(tcp: TcpStream) -> Result<TcpStream> {
    const STARTTLS_TAG: &[u8] = b"MAILGO1";
    let deadline = Instant::now() + STARTTLS_EXCHANGE_TIMEOUT;
    let mut reader = BufReader::new(tcp);
    let mut line = Vec::with_capacity(1024);

    let greeting_bytes = read_bounded_starttls_line(&mut reader, &mut line, |reader| {
        set_starttls_read_deadline(reader.get_ref(), deadline)
    })?;
    if greeting_bytes == 0 {
        return Err(anyhow!(
            "IMAP server closed the connection before its greeting"
        ));
    }
    let mut total_read = greeting_bytes;
    let mut greeting_parts = line.split(|byte| byte.is_ascii_whitespace());
    if greeting_parts.next() != Some(b"*".as_slice())
        || !greeting_parts
            .next()
            .is_some_and(|status| status.eq_ignore_ascii_case(b"OK"))
    {
        return Err(anyhow!(
            "IMAP server did not provide a safe STARTTLS greeting"
        ));
    }

    reader
        .get_mut()
        .write_all(STARTTLS_TAG)
        .and_then(|_| reader.get_mut().write_all(b" STARTTLS\r\n"))
        .and_then(|_| reader.get_mut().flush())
        .context("write IMAP STARTTLS command")?;

    loop {
        let read = read_bounded_starttls_line(&mut reader, &mut line, |reader| {
            set_starttls_read_deadline(reader.get_ref(), deadline)
        })?;
        if read == 0 {
            return Err(anyhow!("IMAP server closed the connection before STARTTLS"));
        }
        total_read = total_read.saturating_add(read);
        if total_read > MAX_STARTTLS_RESPONSE_BYTES {
            return Err(anyhow!("IMAP STARTTLS response is too large"));
        }
        let mut parts = line.split(|byte| byte.is_ascii_whitespace());
        if parts.next() != Some(STARTTLS_TAG) {
            continue;
        }
        let status = parts
            .next()
            .and_then(|value| std::str::from_utf8(value).ok())
            .unwrap_or_default();
        if status.eq_ignore_ascii_case("OK") {
            return Ok(reader.into_inner());
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

/// Authenticate to IMAP and issue NOOP without selecting or downloading a mailbox. The caller
/// owns concurrency control, so diagnostics cannot race an account synchronization lease.
pub fn test_connection(profile: &ProviderProfile, email: &str, credential: &str) -> Result<()> {
    if credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before connecting"));
    }
    let mut session = authenticate(profile, email, credential)?;
    let result = session.noop().context("test IMAP connection");
    session.logout().ok();
    result
}

fn body_session_fingerprint(profile: &ProviderProfile, email: &str, credential: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(profile.provider.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(profile.imap.host.as_bytes());
    hasher.update(profile.imap.port.to_be_bytes());
    hasher.update([match profile.imap.security {
        TransportSecurity::Tls => 1,
        TransportSecurity::StartTls => 2,
    }]);
    hasher.update([match profile.authentication {
        Authentication::OAuth2 => 1,
        Authentication::AppPassword => 2,
        Authentication::Password => 3,
    }]);
    hasher.update(email.trim().to_ascii_lowercase().as_bytes());
    hasher.update([0]);
    hasher.update(credential.as_bytes());
    hasher.finalize().into()
}

fn body_session_slot(account_id: &str) -> Result<BodySessionSlot> {
    let sessions = BODY_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut sessions = sessions
        .lock()
        .map_err(|_| anyhow!("IMAP body session registry lock poisoned"))?;
    Ok(Arc::clone(
        sessions
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None))),
    ))
}

fn clear_body_session(account_id: &str) {
    let Some(sessions) = BODY_SESSIONS.get() else {
        return;
    };
    let slot = sessions
        .lock()
        .ok()
        .and_then(|mut sessions| sessions.remove(account_id));
    if let Some(slot) = slot {
        if let Ok(mut session) = slot.lock() {
            if let Some(mut session) = session.take() {
                session.session.logout().ok();
            }
        }
    }
}

fn fetch_raw_message_with_pooled_session(
    account_id: &str,
    profile: &ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    uid: u32,
) -> Result<(Vec<u8>, bool, bool)> {
    let fingerprint = body_session_fingerprint(profile, email, credential);
    let slot = body_session_slot(account_id)?;
    let mut pooled = slot
        .lock()
        .map_err(|_| anyhow!("IMAP body session lock poisoned"))?;
    let must_reconnect = pooled.as_ref().is_none_or(|session| {
        session.credential_fingerprint != fingerprint
            || session.last_used.elapsed() > BODY_SESSION_IDLE_TTL
    });
    if must_reconnect {
        if let Some(mut stale) = pooled.take() {
            stale.session.logout().ok();
        }
        *pooled = Some(PooledBodySession {
            credential_fingerprint: fingerprint,
            selected_folder: None,
            last_used: Instant::now(),
            session: authenticate(profile, email, credential)?,
        });
    }

    let result = (|| {
        let session = pooled
            .as_mut()
            .ok_or_else(|| anyhow!("IMAP body session is unavailable"))?;
        if session
            .selected_folder
            .as_deref()
            .is_none_or(|selected| !selected.eq_ignore_ascii_case(folder))
        {
            session
                .session
                .select(folder)
                .with_context(|| format!("select {folder}"))?;
            session.selected_folder = Some(folder.to_string());
        }
        let size_probe = session
            .session
            .uid_fetch(uid.to_string(), SIZE_FETCH_QUERY)?;
        let advertised_size = size_probe
            .iter()
            .next()
            .and_then(|item| item.size)
            .ok_or_else(|| anyhow!("message UID {uid} did not include RFC822.SIZE"))?;
        validate_advertised_message_size(advertised_size)?;
        let fetched = session
            .session
            .uid_fetch(uid.to_string(), FULL_FETCH_QUERY)?;
        let item = fetched
            .iter()
            .next()
            .ok_or_else(|| anyhow!("message UID {uid} was not found"))?;
        let raw = item
            .body()
            .ok_or_else(|| anyhow!("message UID {uid} has no RFC822 body"))?;
        if raw.len() > crate::mail::MAX_FULL_MESSAGE_BYTES || raw.len() != advertised_size as usize
        {
            return Err(anyhow!(
                "message UID {uid} returned an invalid or truncated RFC822 body"
            ));
        }
        let raw = raw.to_vec();
        let unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
        let starred = item
            .flags()
            .iter()
            .any(|flag| matches!(flag, Flag::Flagged));
        session.last_used = Instant::now();
        Ok((raw, unread, starred))
    })();

    if result.is_err() {
        if let Some(mut failed) = pooled.take() {
            failed.session.logout().ok();
        }
    }
    result
}

fn detect_incremental_mode(
    session: &mut imap::Session<imap::Connection>,
) -> Option<IncrementalMode> {
    let capabilities = match session.capabilities() {
        Ok(capabilities) => capabilities,
        Err(error) => {
            tracing::debug!("could not read IMAP capabilities for incremental sync: {error}");
            return None;
        }
    };
    if capabilities.has_str("QRESYNC") {
        if session.run_command_and_check_ok("ENABLE QRESYNC").is_ok() {
            return Some(IncrementalMode::Qresync);
        }
        tracing::debug!("IMAP QRESYNC enable failed; falling back to CONDSTORE");
    }
    if capabilities.has_str("CONDSTORE") || capabilities.has_str("QRESYNC") {
        if session.run_command_and_check_ok("ENABLE CONDSTORE").is_ok() {
            Some(IncrementalMode::Condstore)
        } else {
            tracing::debug!("IMAP CONDSTORE enable failed; using the UID sync path");
            None
        }
    } else {
        None
    }
}

fn incremental_status(
    session: &mut imap::Session<imap::Connection>,
    folder: &str,
    mode: Option<IncrementalMode>,
) -> Option<u64> {
    mode?;
    let command = format!("STATUS {} (HIGHESTMODSEQ)", quoted_mailbox_name(folder));
    match session.run_command_and_read_response(command) {
        Ok(response) => parse_status_highest_mod_seq(&response, folder),
        Err(error) => {
            tracing::debug!(folder = %folder, "IMAP incremental status unavailable: {error}");
            None
        }
    }
}

fn parse_status_highest_mod_seq(response: &[u8], expected_folder: &str) -> Option<u64> {
    for line in response.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(rest) = line.strip_prefix(b"* STATUS ") else {
            continue;
        };
        let (mailbox, attributes) = if rest.first() == Some(&b'"') {
            let mut value = Vec::new();
            let mut escaped = false;
            let mut end = None;
            for (index, byte) in rest.iter().enumerate().skip(1) {
                if escaped {
                    value.push(*byte);
                    escaped = false;
                } else if *byte == b'\\' {
                    escaped = true;
                } else if *byte == b'"' {
                    end = Some(index);
                    break;
                } else {
                    value.push(*byte);
                }
            }
            let end = end?;
            let attribute_start = rest[end + 1..]
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())?;
            let attributes = &rest[end + 1 + attribute_start..];
            (String::from_utf8(value).ok()?, attributes)
        } else {
            let separator = rest.iter().position(|byte| byte.is_ascii_whitespace())?;
            let attribute_start = rest[separator..]
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())?;
            (
                String::from_utf8(rest[..separator].to_vec()).ok()?,
                &rest[separator + attribute_start..],
            )
        };
        if mailbox != expected_folder {
            continue;
        }
        let marker = b"HIGHESTMODSEQ ";
        let start = attributes
            .windows(marker.len())
            .position(|window| window.eq_ignore_ascii_case(marker))?
            + marker.len();
        let digits = attributes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .copied()
            .collect::<Vec<_>>();
        if digits.is_empty() {
            return None;
        }
        return std::str::from_utf8(&digits).ok()?.parse().ok();
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleLoopExit {
    MailboxChanged,
    Unsupported,
    Paused,
    Removed,
}

enum IdleAccountState {
    Ready(Box<crate::PersistedAccount>),
    Paused,
    Removed,
}

fn idle_watch_enabled(offline_mode: bool, status: &str) -> bool {
    !offline_mode && !matches!(status, "needs-auth" | "syncing")
}

fn idle_account_state(
    shared: &Arc<Mutex<crate::MailGoState>>,
    account_id: &str,
) -> Result<IdleAccountState> {
    let app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    let Some(account) = app
        .state
        .accounts
        .iter()
        .find(|account| account.id == account_id)
        .cloned()
    else {
        return Ok(IdleAccountState::Removed);
    };
    if idle_watch_enabled(app.state.offline_mode, &account.status) {
        Ok(IdleAccountState::Ready(Box::new(account)))
    } else {
        Ok(IdleAccountState::Paused)
    }
}

fn idle_retry_delay(attempt: u32) -> Duration {
    let multiplier = 1_u64 << attempt.min(5);
    Duration::from_secs(
        IDLE_RETRY_BASE_SECONDS
            .saturating_mul(multiplier)
            .min(IDLE_RETRY_MAX_SECONDS),
    )
}

fn prepare_idle_session<T: Read + Write>(session: &mut imap::Session<T>) -> Result<bool> {
    let capabilities = session
        .capabilities()
        .context("read IMAP capabilities before IDLE")?;
    if !capabilities.has_str("IDLE") {
        return Ok(false);
    }
    session
        .select("INBOX")
        .context("select INBOX before IMAP IDLE")?;
    Ok(true)
}

fn wait_for_idle_signal<T>(session: &mut imap::Session<T>, timeout: Duration) -> Result<WaitOutcome>
where
    T: Read + Write + SetReadTimeout,
{
    let mut handle = session.idle();
    handle.timeout(timeout).keepalive(false);
    handle
        .wait_while(idle::stop_on_any)
        .context("wait for IMAP IDLE update")
}

fn wait_for_account_idle_event(
    shared: &Arc<Mutex<crate::MailGoState>>,
    account_id: &str,
    profile: &ProviderProfile,
    email: &str,
    credential: &str,
) -> Result<IdleLoopExit> {
    let mut session = authenticate(profile, email, credential)?;
    if !prepare_idle_session(&mut session)? {
        session.logout().ok();
        return Ok(IdleLoopExit::Unsupported);
    }
    tracing::info!(account_id = %account_id, provider = profile.provider.as_str(), "IMAP IDLE listener ready");

    loop {
        if wait_for_idle_signal(&mut session, IDLE_STATE_CHECK_INTERVAL)?
            == WaitOutcome::MailboxChanged
        {
            session.logout().ok();
            return Ok(IdleLoopExit::MailboxChanged);
        }
        match idle_account_state(shared, account_id)? {
            IdleAccountState::Ready(_) => {}
            IdleAccountState::Paused => {
                session.logout().ok();
                return Ok(IdleLoopExit::Paused);
            }
            IdleAccountState::Removed => {
                session.logout().ok();
                return Ok(IdleLoopExit::Removed);
            }
        }
    }
}

fn run_background_sync(
    shared: &Arc<Mutex<crate::MailGoState>>,
    cache_root: &Path,
    account: &crate::PersistedAccount,
    trigger: &'static str,
) -> bool {
    match shared.lock() {
        Ok(app) if app.state.offline_mode => {
            tracing::debug!(account_id = %account.id, trigger, "background sync skipped because offline-only mode is enabled");
            return true;
        }
        Err(_) => {
            tracing::warn!(account_id = %account.id, trigger, "background sync state lock poisoned");
            return true;
        }
        Ok(_) => {}
    }
    let _sync_lease = match crate::try_begin_account_sync(shared, &account.id) {
        Ok(lease) => lease,
        Err(error) => {
            tracing::debug!(
                account_id = %account.id,
                trigger,
                "background sync skipped because another operation owns the account: {error}"
            );
            return false;
        }
    };
    let profile = match crate::profile_for_account(account) {
        Ok(profile) => profile,
        Err(error) => {
            tracing::warn!(account_id = %account.id, trigger, "background provider profile is invalid: {error}");
            return true;
        }
    };
    let credential = match crate::load_credential(account) {
        Ok(credential) => credential,
        Err(error) => {
            crate::record_account_sync_failure(shared, &account.id, needs_reauthorization(&error));
            tracing::warn!(
                account_id = %account.id,
                trigger,
                needs_auth = needs_reauthorization(&error),
                "background credential load failed"
            );
            return true;
        }
    };
    if crate::outbox::flush_due(
        cache_root,
        &account.id,
        profile.clone(),
        &account.email,
        &credential,
    )
    .is_err()
    {
        tracing::warn!(account_id = %account.id, trigger, "background outbox flush failed");
    }
    let provider = profile.provider;
    match sync_account(
        &account.id,
        profile,
        &account.email,
        &credential,
        cache_root,
    ) {
        Ok(result) => {
            let notification = if let Ok(mut app) = shared.lock() {
                let previous_unread = app
                    .state
                    .accounts
                    .iter()
                    .find(|stored| stored.id == account.id)
                    .map(|stored| stored.unread as usize)
                    .unwrap_or(account.unread as usize);
                let notification = (app.state.notifications_enabled
                    && result.unread > previous_unread)
                    .then(|| {
                        (
                            account.label.clone(),
                            result.unread.saturating_sub(previous_unread),
                        )
                    });
                crate::record_account_sync_success(&mut app, &result);
                if let Some(stored) = app
                    .state
                    .accounts
                    .iter_mut()
                    .find(|stored| stored.id == account.id)
                {
                    stored.last_sync = "后台刚刚同步".into();
                }
                if let Err(error) = app.save() {
                    tracing::warn!(account_id = %account.id, trigger, "background sync state save failed: {error}");
                }
                notification
            } else {
                tracing::warn!(account_id = %account.id, trigger, "background sync state lock poisoned");
                None
            };
            if let Some((label, count)) = notification {
                crate::tray::notify_new_mail("MailGo", &format!("{label} 有 {count} 封未读邮件"));
            }
            tracing::info!(account_id = %account.id, provider = provider.as_str(), trigger, unread = result.unread, "background sync completed");
        }
        Err(error) => {
            crate::record_account_sync_failure(shared, &account.id, needs_reauthorization(&error));
            tracing::warn!(
                account_id = %account.id,
                trigger,
                category = error_category(&error, provider),
                detail = error_detail(&error),
                "background sync failed"
            );
        }
    }
    true
}

fn run_idle_triggered_sync(
    shared: &Arc<Mutex<crate::MailGoState>>,
    cache_root: &Path,
    account_id: &str,
) {
    for retry in 0..=IDLE_SYNC_BUSY_RETRIES {
        let account = match idle_account_state(shared, account_id) {
            Ok(IdleAccountState::Ready(account)) => account,
            Ok(IdleAccountState::Paused | IdleAccountState::Removed) | Err(_) => return,
        };
        if run_background_sync(shared, cache_root, &account, "idle") {
            return;
        }
        if retry < IDLE_SYNC_BUSY_RETRIES {
            thread::sleep(IDLE_PAUSED_RECHECK);
        }
    }
    tracing::debug!(account_id = %account_id, "IMAP IDLE update was covered by a longer in-flight sync");
}

fn run_idle_worker(
    shared: Arc<Mutex<crate::MailGoState>>,
    cache_root: PathBuf,
    account_id: String,
) {
    let mut retry_attempt = 0u32;
    loop {
        let account = match idle_account_state(&shared, &account_id) {
            Ok(IdleAccountState::Ready(account)) => account,
            Ok(IdleAccountState::Paused) => {
                thread::sleep(IDLE_PAUSED_RECHECK);
                continue;
            }
            Ok(IdleAccountState::Removed) => return,
            Err(error) => {
                tracing::warn!(account_id = %account_id, "IMAP IDLE worker stopped: {error}");
                return;
            }
        };
        let profile = match crate::profile_for_account(&account) {
            Ok(profile) => profile,
            Err(error) => {
                tracing::warn!(account_id = %account_id, "IMAP IDLE profile is invalid: {error}");
                thread::sleep(IDLE_UNSUPPORTED_RECHECK);
                continue;
            }
        };
        let credential = match crate::load_credential(&account) {
            Ok(credential) => credential,
            Err(error) => {
                crate::record_account_sync_failure(
                    &shared,
                    &account_id,
                    needs_reauthorization(&error),
                );
                tracing::warn!(account_id = %account_id, needs_auth = needs_reauthorization(&error), "IMAP IDLE credential load failed");
                thread::sleep(IDLE_PAUSED_RECHECK);
                continue;
            }
        };
        let provider = profile.provider;
        match wait_for_account_idle_event(
            &shared,
            &account_id,
            &profile,
            &account.email,
            &credential,
        ) {
            Ok(IdleLoopExit::MailboxChanged) => {
                retry_attempt = 0;
                tracing::info!(account_id = %account_id, provider = provider.as_str(), "IMAP IDLE mailbox change received");
                run_idle_triggered_sync(&shared, &cache_root, &account_id);
            }
            Ok(IdleLoopExit::Unsupported) => {
                retry_attempt = 0;
                tracing::info!(account_id = %account_id, provider = provider.as_str(), "IMAP IDLE unavailable; periodic sync remains active");
                thread::sleep(IDLE_UNSUPPORTED_RECHECK);
            }
            Ok(IdleLoopExit::Paused) => {
                retry_attempt = 0;
                thread::sleep(IDLE_PAUSED_RECHECK);
            }
            Ok(IdleLoopExit::Removed) => return,
            Err(error) => {
                let delay = idle_retry_delay(retry_attempt);
                retry_attempt = retry_attempt.saturating_add(1);
                tracing::warn!(
                    account_id = %account_id,
                    provider = provider.as_str(),
                    category = error_category(&error, provider),
                    detail = error_detail(&error),
                    retry_after = delay.as_secs(),
                    "IMAP IDLE listener interrupted; reconnecting"
                );
                thread::sleep(delay);
            }
        }
    }
}

fn spawn_idle_supervisor(shared: Arc<Mutex<crate::MailGoState>>, cache_root: PathBuf) {
    let result = thread::Builder::new()
        .name("mailgo-idle-supervisor".into())
        .spawn(move || {
            let (finished_sender, finished_receiver) = mpsc::channel::<String>();
            let mut active_workers = HashSet::new();
            loop {
                while let Ok(account_id) = finished_receiver.try_recv() {
                    active_workers.remove(&account_id);
                }
                let account_ids = match shared.lock() {
                    Ok(app) => app
                        .state
                        .accounts
                        .iter()
                        .map(|account| account.id.clone())
                        .collect::<Vec<_>>(),
                    Err(_) => {
                        tracing::warn!("IMAP IDLE supervisor state lock poisoned");
                        thread::sleep(IDLE_SUPERVISOR_INTERVAL);
                        continue;
                    }
                };
                for account_id in account_ids {
                    if !active_workers.insert(account_id.clone()) {
                        continue;
                    }
                    let worker_shared = Arc::clone(&shared);
                    let worker_cache_root = cache_root.clone();
                    let worker_sender = finished_sender.clone();
                    let worker_id = account_id.clone();
                    let thread_suffix = account_id
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .take(12)
                        .collect::<String>();
                    if let Err(error) = thread::Builder::new()
                        .name(format!("mailgo-idle-{thread_suffix}"))
                        .spawn(move || {
                            run_idle_worker(worker_shared, worker_cache_root, worker_id.clone());
                            let _ = worker_sender.send(worker_id);
                        })
                    {
                        active_workers.remove(&account_id);
                        tracing::warn!(account_id = %account_id, "could not start IMAP IDLE worker: {error}");
                    }
                }
                thread::sleep(IDLE_SUPERVISOR_INTERVAL);
            }
        });
    if let Err(error) = result {
        tracing::warn!("could not start IMAP IDLE supervisor: {error}");
    }
}

/// Keep the local cache fresh while the window is hidden. The scheduler intentionally runs on a
/// dedicated thread so IMAP handshakes never block rdesktop's WebView event loop.
pub fn spawn_scheduler(shared: Arc<Mutex<crate::MailGoState>>, cache_root: PathBuf) {
    spawn_outbox_scheduler(Arc::clone(&shared), cache_root.clone());
    spawn_idle_supervisor(Arc::clone(&shared), cache_root.clone());
    thread::Builder::new()
        .name("mailgo-sync-scheduler".into())
        .spawn(move || {
            let mut first_run = true;
            loop {
                thread::sleep(if first_run {
                    INITIAL_SYNC_DELAY
                } else {
                    BACKGROUND_SYNC_INTERVAL
                });
                first_run = false;
                let (accounts, offline_mode) = match shared.lock() {
                    Ok(app) => (app.state.accounts.clone(), app.state.offline_mode),
                    Err(_) => {
                        tracing::warn!("background sync state lock poisoned");
                        continue;
                    }
                };
                if offline_mode {
                    tracing::debug!("background sync skipped because offline-only mode is enabled");
                    continue;
                }
                map_with_concurrency(&accounts, ACCOUNT_SYNC_CONCURRENCY, |account| {
                    run_background_sync(&shared, &cache_root, account, "periodic")
                });
            }
        })
        .expect("start MailGo sync scheduler");
}

fn run_scheduled_outbox_account(
    shared: &Arc<Mutex<crate::MailGoState>>,
    cache_root: &Path,
    account_id: &str,
) {
    let account = match crate::account_for(shared, account_id) {
        Ok(account) => account,
        Err(_) => {
            if let Err(error) = crate::outbox::remove_account(cache_root, account_id) {
                tracing::warn!(account_id = %account_id, "could not remove orphaned outbox entries: {error}");
            }
            return;
        }
    };
    let _sync_lease = match crate::try_begin_account_sync(shared, account_id) {
        Ok(lease) => lease,
        Err(_) => return,
    };
    let profile = match crate::profile_for_account(&account) {
        Ok(profile) => profile,
        Err(_) => {
            if let Err(error) = crate::outbox::pause_account(
                cache_root,
                account_id,
                "账户设置无效，请重新配置后再发送",
            ) {
                tracing::warn!(account_id = %account_id, "could not pause invalid-account outbox: {error}");
            }
            return;
        }
    };
    let credential = match crate::load_credential(&account) {
        Ok(credential) => credential,
        Err(error) => {
            let needs_auth = needs_reauthorization(&error);
            crate::record_account_sync_failure(shared, account_id, needs_auth);
            if needs_auth {
                if let Err(pause_error) = crate::outbox::pause_account(
                    cache_root,
                    account_id,
                    "账户需要重新授权后才能发送",
                ) {
                    tracing::warn!(account_id = %account_id, "could not pause unauthorized outbox: {pause_error}");
                }
            }
            return;
        }
    };
    match crate::outbox::flush_due(cache_root, account_id, profile, &account.email, &credential) {
        Ok(summary) if summary.authentication_failed => {
            crate::record_account_sync_failure(shared, account_id, true);
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(account_id = %account_id, "scheduled outbox flush failed: {error}");
        }
    }
}

fn spawn_outbox_scheduler(shared: Arc<Mutex<crate::MailGoState>>, cache_root: PathBuf) {
    let result = thread::Builder::new()
        .name("mailgo-outbox-scheduler".into())
        .spawn(move || loop {
            let observed = crate::outbox::scheduler_generation();
            let offline_mode = match shared.lock() {
                Ok(app) => app.state.offline_mode,
                Err(_) => {
                    tracing::warn!("outbox scheduler state lock poisoned");
                    crate::outbox::wait_for_scheduler_change(observed, OUTBOX_SCHEDULER_BUSY_RETRY);
                    continue;
                }
            };
            if offline_mode {
                crate::outbox::wait_for_scheduler_change(observed, OUTBOX_SCHEDULER_IDLE);
                continue;
            }

            let next_delay = match crate::outbox::next_due_delay(&cache_root) {
                Ok(delay) => delay,
                Err(error) => {
                    tracing::warn!("could not inspect encrypted outbox schedule: {error}");
                    crate::outbox::wait_for_scheduler_change(observed, OUTBOX_SCHEDULER_BUSY_RETRY);
                    continue;
                }
            };
            match next_delay {
                None => {
                    crate::outbox::wait_for_scheduler_change(observed, OUTBOX_SCHEDULER_IDLE);
                    continue;
                }
                Some(delay) if !delay.is_zero() => {
                    crate::outbox::wait_for_scheduler_change(
                        observed,
                        delay.min(OUTBOX_SCHEDULER_IDLE),
                    );
                    continue;
                }
                Some(_) => {}
            }

            let account_ids = match crate::outbox::due_account_ids(&cache_root) {
                Ok(account_ids) => account_ids,
                Err(error) => {
                    tracing::warn!("could not read due outbox accounts: {error}");
                    crate::outbox::wait_for_scheduler_change(observed, OUTBOX_SCHEDULER_BUSY_RETRY);
                    continue;
                }
            };
            map_with_concurrency(&account_ids, ACCOUNT_SYNC_CONCURRENCY, |account_id| {
                run_scheduled_outbox_account(&shared, &cache_root, account_id)
            });
            crate::outbox::wait_for_scheduler_change(observed, OUTBOX_SCHEDULER_BUSY_RETRY);
        });
    if let Err(error) = result {
        tracing::warn!("could not start outbox scheduler: {error}");
    }
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
    let provider = profile.provider;
    let mut attempt = 0u32;
    loop {
        match sync_account_once(account_id, profile.clone(), email, credential, cache_root) {
            Ok(result) => return Ok(result),
            Err(error) if attempt < 2 => {
                let Some(delay) = retry_delay(&error, provider, attempt) else {
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
fn highest_mailbox_uid(
    session: &mut imap::Session<imap::Connection>,
    uid_next: Option<u32>,
    exists: u32,
    context: &str,
) -> Result<u32> {
    if let Some(uid_next) = uid_next {
        return Ok(uid_next.saturating_sub(1));
    }
    if exists == 0 {
        return Ok(0);
    }

    let fetched = session
        .fetch(exists.to_string(), "UID")
        .with_context(|| context.to_string())?;
    if fetched.len() > 1 {
        return Err(anyhow!("IMAP highest-UID probe returned too many messages"));
    }
    fetched
        .iter()
        .filter_map(|item| item.uid)
        .max()
        .ok_or_else(|| anyhow!("IMAP server omitted UIDNEXT and the highest message UID"))
}

fn next_uid_search_range(
    uid_floor: u32,
    uid_ceiling: u32,
    cursor: u32,
    order: UidSearchOrder,
) -> Option<(u32, u32, bool)> {
    if uid_ceiling == 0 || uid_floor == 0 || uid_floor > uid_ceiling {
        return None;
    }
    Some(match order {
        UidSearchOrder::Ascending => {
            let range_floor = cursor.max(uid_floor);
            if range_floor > uid_ceiling {
                return None;
            }
            let range_ceiling = range_floor
                .saturating_add(MAX_UID_SEARCH_RANGE_WIDTH.saturating_sub(1))
                .min(uid_ceiling);
            (range_floor, range_ceiling, range_ceiling < uid_ceiling)
        }
        UidSearchOrder::Descending => {
            let range_ceiling = cursor.min(uid_ceiling);
            if range_ceiling < uid_floor {
                return None;
            }
            let range_floor = range_ceiling
                .saturating_sub(MAX_UID_SEARCH_RANGE_WIDTH.saturating_sub(1))
                .max(uid_floor);
            (range_floor, range_ceiling, range_floor > uid_floor)
        }
    })
}

fn search_uid_window(
    session: &mut imap::Session<imap::Connection>,
    uid_floor: u32,
    uid_ceiling: u32,
    criteria: Option<&str>,
    limit: usize,
    order: UidSearchOrder,
    context: &str,
) -> Result<BoundedUidSearch> {
    if uid_ceiling == 0 || uid_floor > uid_ceiling {
        return Ok(BoundedUidSearch::default());
    }
    let uid_floor = uid_floor.max(1);
    let limit = limit.clamp(1, MAX_UIDS_PER_QUERY);
    let mut uids = Vec::with_capacity(limit.min(MAX_UID_SEARCH_RANGE_WIDTH as usize));
    let mut cursor = match order {
        UidSearchOrder::Ascending => uid_floor,
        UidSearchOrder::Descending => uid_ceiling,
    };
    let criteria = criteria.filter(|value| !value.is_empty());

    while let Some((range_floor, range_ceiling, has_another_range)) =
        next_uid_search_range(uid_floor, uid_ceiling, cursor, order)
    {
        let query = match criteria {
            Some(criteria) => format!("UID {range_floor}:{range_ceiling} {criteria}"),
            None => format!("UID {range_floor}:{range_ceiling}"),
        };
        let batch = session
            .uid_search(query)
            .with_context(|| context.to_string())?;
        let range_width = range_ceiling.saturating_sub(range_floor) as usize + 1;
        if batch.len() > range_width
            || batch
                .iter()
                .any(|uid| *uid < range_floor || *uid > range_ceiling)
        {
            return Err(anyhow!(
                "IMAP UID search response escaped the requested bounded range"
            ));
        }
        uids.extend(batch);

        if uids.len() >= limit {
            let truncated = uids.len() > limit || has_another_range;
            uids.sort_unstable();
            if order == UidSearchOrder::Descending {
                uids.reverse();
            }
            uids.truncate(limit);
            return Ok(BoundedUidSearch { uids, truncated });
        }
        if !has_another_range {
            break;
        }
        cursor = match order {
            UidSearchOrder::Ascending => range_ceiling.saturating_add(1),
            UidSearchOrder::Descending => range_floor.saturating_sub(1),
        };
    }

    uids.sort_unstable();
    if order == UidSearchOrder::Descending {
        uids.reverse();
    }
    Ok(BoundedUidSearch {
        uids,
        truncated: false,
    })
}

fn select_latest_uids(
    recent_search: &BoundedUidSearch,
    uid_search_floor: u32,
    newest_cached_uid: Option<u32>,
) -> LatestUidSelection {
    let Some(newest_uid) = newest_cached_uid else {
        return LatestUidSelection {
            selected_uids: recent_search
                .uids
                .iter()
                .copied()
                .take(MAX_HEADER_MESSAGES)
                .collect(),
            new_uid_count: None,
            rebase_to_latest: true,
        };
    };

    let mut new_uids = recent_search
        .uids
        .iter()
        .copied()
        .filter(|uid| *uid > newest_uid)
        .collect::<Vec<_>>();
    let crossed_cached_boundary = recent_search.uids.iter().any(|uid| *uid <= newest_uid);
    let search_reached_cached_window = uid_search_floor <= newest_uid.saturating_add(1)
        && (!recent_search.truncated || crossed_cached_boundary);
    if !new_uids.is_empty() && !search_reached_cached_window {
        return LatestUidSelection {
            selected_uids: recent_search
                .uids
                .iter()
                .copied()
                .take(MAX_HEADER_MESSAGES)
                .collect(),
            new_uid_count: None,
            rebase_to_latest: true,
        };
    }

    new_uids.sort_unstable();
    LatestUidSelection {
        selected_uids: new_uids.into_iter().take(MAX_HEADER_MESSAGES).collect(),
        new_uid_count: Some(if search_reached_cached_window {
            recent_search
                .uids
                .iter()
                .filter(|uid| **uid > newest_uid)
                .count()
        } else {
            MAX_HEADER_MESSAGES + 1
        }),
        rebase_to_latest: false,
    }
}

pub fn search_account(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    query: &str,
    limit: usize,
    cache_root: &Path,
) -> Result<SearchResult> {
    retry_imap_operation(account_id, "server search", profile.provider, || {
        search_account_once(
            account_id,
            profile.clone(),
            email,
            credential,
            query,
            limit,
            cache_root,
        )
    })
}

fn search_account_once(
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
        let remaining = limit.saturating_sub(messages.len());
        let folder_limit = remaining.min(MAX_SEARCH_RESULTS_PER_FOLDER);
        let highest_uid = highest_mailbox_uid(
            &mut session,
            mailbox.uid_next,
            mailbox.exists,
            &format!("probe highest UID in {folder}"),
        )?;
        if highest_uid == 0 {
            continue;
        }
        let uid_floor = highest_uid
            .saturating_sub(MAX_UID_SEARCH_WINDOW.saturating_sub(1))
            .max(1);
        let search = search_uid_window(
            &mut session,
            uid_floor,
            highest_uid,
            Some(&query),
            folder_limit,
            UidSearchOrder::Descending,
            &format!("search {folder}"),
        )?;
        truncated |= search.truncated;
        let selected_uids = search.uids;
        if selected_uids.is_empty() {
            continue;
        }
        let folder_messages = fetch_header_messages(
            &mut session,
            account_id,
            &folder,
            &selected_uids,
            &format!("fetch search results in {folder}"),
        )?;
        if folder_messages.len() < selected_uids.len() {
            truncated = true;
        }
        save_search_messages(
            cache_root,
            account_id,
            mailbox.uid_validity,
            &folder_messages,
        )?;
        messages.extend(folder_messages);
    }
    crate::cache_db::spawn_search_indexer(cache_root.to_path_buf());
    session.logout().ok();
    messages.sort_by(|left, right| right.received_at.cmp(&left.received_at));
    Ok(SearchResult {
        messages,
        truncated,
        folders_searched,
    })
}

fn fetch_header_messages(
    session: &mut imap::Session<imap::Connection>,
    account_id: &str,
    folder: &str,
    uids: &[u32],
    context: &str,
) -> Result<Vec<CachedMessage>> {
    let mut messages = Vec::with_capacity(uids.len());
    let mut header_bytes = 0usize;
    for batch in uids.chunks(MAX_HEADER_FETCH_BATCH) {
        let uid_set = batch
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        if uid_set.is_empty() {
            continue;
        }
        let fetched = session
            .uid_fetch(uid_set, HEADER_FETCH_QUERY)
            .with_context(|| context.to_string())?;
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
                return Ok(messages);
            }
            let unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
            let starred = item
                .flags()
                .iter()
                .any(|flag| matches!(flag, Flag::Flagged));
            if let Ok(message) = parse_header(account_id, folder, uid, unread, starred, header) {
                messages.push(message);
            }
        }
    }
    Ok(messages)
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
    let incremental_mode = detect_incremental_mode(&mut session);

    flush_queued_moves(&mut session, cache_root, account_id, profile.provider);
    flush_queued_flags(&mut session, cache_root, account_id);

    let synced_at = now_stamp();
    let mut synced_folders = Vec::new();
    let mut inbox_fetched = 0usize;
    let mut inbox_unread = 0usize;
    let mut cache_path = cache_root.join(safe_component(account_id)).join(CACHE_FILE);
    let mut metadata_only_folders = 0usize;
    let mut exact_delta_folders = 0usize;
    let mut full_rewrite_folders = 0usize;

    for folder in discover_folders(&mut session, profile.provider) {
        let status_highest_mod_seq = incremental_status(&mut session, &folder, incremental_mode);
        let Ok(mailbox) = session.select(&folder) else {
            continue;
        };
        let outcome = sync_folder_latest(
            &mut session,
            account_id,
            &folder,
            mailbox.uid_validity,
            mailbox.uid_next,
            mailbox.exists,
            mailbox.highest_mod_seq.or(status_highest_mod_seq),
            incremental_mode,
            cache_root,
            synced_at.clone(),
        )?;
        let path = if outcome.force_full_rewrite {
            full_rewrite_folders = full_rewrite_folders.saturating_add(1);
            save_mailbox(cache_root, account_id, &outcome.mailbox)?
        } else {
            if outcome.delta.is_empty() {
                metadata_only_folders = metadata_only_folders.saturating_add(1);
            } else {
                exact_delta_folders = exact_delta_folders.saturating_add(1);
            }
            save_mailbox_delta(cache_root, account_id, &outcome.mailbox, &outcome.delta)?
        };
        if folder.eq_ignore_ascii_case("INBOX") {
            cache_path = path;
            inbox_fetched = outcome.fetched;
            inbox_unread = outcome
                .mailbox
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

    let folder_labels = folder_display_labels(&synced_folders);
    tracing::info!(
        account_id = %account_id,
        metadata_only_folders,
        exact_delta_folders,
        full_rewrite_folders,
        "mailbox cache persistence completed"
    );
    crate::cache_db::spawn_search_indexer(cache_root.to_path_buf());
    if let Err(error) = crate::cache_db::refresh_backup(cache_root) {
        tracing::warn!(
            error = %error,
            "could not refresh the indexed mail cache recovery copy"
        );
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
        folder_labels,
    })
}

#[allow(clippy::too_many_arguments)]
fn sync_folder_latest(
    session: &mut imap::Session<imap::Connection>,
    account_id: &str,
    folder: &str,
    uid_validity: Option<u32>,
    uid_next: Option<u32>,
    exists: u32,
    highest_mod_seq: Option<u64>,
    incremental_mode: Option<IncrementalMode>,
    cache_root: &Path,
    synced_at: String,
) -> Result<FolderSyncOutcome> {
    validate_mailbox_name(folder)?;
    let cached_state = load_mailbox_for_sync(cache_root, account_id, folder)?;
    let mut force_full_rewrite = cached_state.is_none();
    let mut cached = cached_state.unwrap_or_else(|| CachedMailbox::empty(account_id, folder));
    let cache_reparse_required = upgrade_mailbox_cache(&mut cached);
    let uid_validity_changed = cached.uid_validity.is_some() && cached.uid_validity != uid_validity;
    if uid_validity_changed || cache_reparse_required {
        force_full_rewrite = true;
        reset_mailbox_sync_window(&mut cached);
    } else if let (Some(mode), Some(current), Some(previous)) =
        (incremental_mode, highest_mod_seq, cached.highest_mod_seq)
    {
        let highest_possible_uid = highest_mailbox_uid(
            session,
            uid_next,
            exists,
            &format!("probe highest UID in {folder}"),
        )?;
        if let Some(delta) = sync_folder_incremental(
            session,
            account_id,
            folder,
            uid_validity,
            current,
            previous,
            highest_possible_uid,
            mode,
            &mut cached,
            synced_at.clone(),
        )? {
            return Ok(FolderSyncOutcome {
                fetched: delta.new_messages.len(),
                mailbox: cached,
                delta,
                force_full_rewrite,
            });
        }
    }

    let mut delta = MailboxSyncDelta::default();
    let newest_cached_uid = cached.messages.iter().map(|message| message.uid).max();
    let highest_possible_uid = highest_mailbox_uid(
        session,
        uid_next,
        exists,
        &format!("probe highest UID in {folder}"),
    )?;
    let uid_search_floor = highest_possible_uid
        .saturating_sub(MAX_UID_SEARCH_WINDOW.saturating_sub(1))
        .max(1);
    let recent_search = if highest_possible_uid == 0 {
        BoundedUidSearch::default()
    } else {
        search_uid_window(
            session,
            uid_search_floor,
            highest_possible_uid,
            None,
            MAX_HEADER_MESSAGES + 1,
            UidSearchOrder::Descending,
            &format!("search recent UIDs in {folder}"),
        )?
    };

    // A cold cache and a large post-offline backlog both rebase onto the newest visible page.
    // This avoids waiting through one hundred-message catch-up cycles before showing current mail;
    // older rows remain server-backed and are recovered by the existing waterfall paginator.
    let selection = select_latest_uids(&recent_search, uid_search_floor, newest_cached_uid);
    let rebase_to_latest = selection.rebase_to_latest;
    let selected_uids = selection.selected_uids;
    let new_uid_count = selection.new_uid_count;

    let server_has_more;
    let current_uids;
    if rebase_to_latest {
        if newest_cached_uid.is_some() {
            force_full_rewrite = true;
            reset_mailbox_sync_window(&mut cached);
            delta = MailboxSyncDelta::default();
        }
        server_has_more = recent_search.truncated || uid_search_floor > 1;
        current_uids = HashSet::new();
    } else {
        server_has_more = cached.has_more;
        let requested_uids = cached
            .messages
            .iter()
            .map(|message| message.uid)
            .collect::<HashSet<_>>();
        current_uids = if let Some(cached_uid_set) = cached_uid_set(&cached) {
            let uids = session
                .uid_search(format!("UID {cached_uid_set}"))
                .with_context(|| format!("check cached UIDs in {folder}"))?;
            if uids.len() > requested_uids.len()
                || uids.iter().any(|uid| !requested_uids.contains(uid))
            {
                return Err(anyhow!(
                    "IMAP cached-UID search returned an unrequested message"
                ));
            }
            uids.into_iter().collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
    }
    let mut fetched_messages = fetch_header_messages(
        session,
        account_id,
        folder,
        &selected_uids,
        &format!("fetch {folder} message headers"),
    )?;
    fetched_messages.sort_by_key(|message| std::cmp::Reverse(message.uid));

    delta.removed_uids.extend(
        cached
            .messages
            .iter()
            .filter(|message| !current_uids.contains(&message.uid))
            .map(|message| message.uid),
    );
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
            .uid_fetch(refresh_uid_set, FLAGS_FETCH_QUERY)
            .with_context(|| format!("refresh {folder} message flags"))?;
        for item in fetched.iter() {
            let Some(uid) = item.uid else { continue };
            if let Some(message) = cached
                .messages
                .iter_mut()
                .find(|message| message.uid == uid)
            {
                let unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
                let starred = item
                    .flags()
                    .iter()
                    .any(|flag| matches!(flag, Flag::Flagged));
                if message.unread != unread || message.starred != starred {
                    message.unread = unread;
                    message.starred = starred;
                    delta.flag_updates.push((uid, unread, starred));
                }
            }
        }
    }

    let fetched_uids = fetched_messages
        .iter()
        .map(|message| message.uid)
        .collect::<HashSet<_>>();
    let all_selected_headers_fetched = selected_uids.iter().all(|uid| fetched_uids.contains(uid));
    let delta_headers_complete = new_uid_count
        .map(|count| count <= MAX_HEADER_MESSAGES)
        .unwrap_or(all_selected_headers_fetched)
        && all_selected_headers_fetched;
    cached.uid_validity = uid_validity;
    cached.highest_mod_seq = if delta_headers_complete {
        highest_mod_seq
    } else {
        cached.highest_mod_seq
    };
    cached.synced_at = synced_at;
    cached
        .messages
        .retain(|message| !fetched_messages.iter().any(|item| item.uid == message.uid));
    cached.messages.extend(fetched_messages.iter().cloned());
    cached
        .messages
        .sort_by_key(|message| std::cmp::Reverse(message.uid));
    cached.oldest_uid = cached.messages.iter().map(|message| message.uid).min();
    cached.has_more = server_has_more;
    let fetched = fetched_messages.len();
    delta.new_messages = fetched_messages;
    Ok(FolderSyncOutcome {
        mailbox: cached,
        fetched,
        delta,
        force_full_rewrite,
    })
}

fn delta_fetch_query(mode: IncrementalMode, previous_mod_seq: u64) -> String {
    let vanished = if mode == IncrementalMode::Qresync {
        " VANISHED"
    } else {
        ""
    };
    format!("(UID FLAGS MODSEQ) (CHANGEDSINCE {previous_mod_seq}{vanished})")
}

fn cached_uid_set(cached: &CachedMailbox) -> Option<String> {
    let mut uids = cached
        .messages
        .iter()
        .map(|message| message.uid)
        .collect::<Vec<_>>();
    uids.sort_unstable();
    uids.dedup();
    (!uids.is_empty()).then(|| {
        uids.into_iter()
            .map(|uid| uid.to_string())
            .collect::<Vec<_>>()
            .join(",")
    })
}

/// Apply a bounded CONDSTORE/QRESYNC delta. Returning `None` means the server advertised an
/// extension but rejected one of its commands; the caller must immediately use the safe UID
/// implementation on the same sync attempt. Cache mutation is delayed until all required wire
/// responses have been parsed so a failed extension path cannot persist a half-applied snapshot.
#[allow(clippy::too_many_arguments)]
fn sync_folder_incremental(
    session: &mut imap::Session<imap::Connection>,
    account_id: &str,
    folder: &str,
    uid_validity: Option<u32>,
    current_mod_seq: u64,
    previous_mod_seq: u64,
    highest_possible_uid: u32,
    mode: IncrementalMode,
    cached: &mut CachedMailbox,
    synced_at: String,
) -> Result<Option<MailboxSyncDelta>> {
    if current_mod_seq < previous_mod_seq {
        return Ok(None);
    }
    if current_mod_seq == previous_mod_seq {
        cached.uid_validity = uid_validity;
        cached.highest_mod_seq = Some(current_mod_seq);
        cached.synced_at = synced_at;
        return Ok(Some(MailboxSyncDelta::default()));
    }

    let Some(cached_uid_set) = cached_uid_set(cached) else {
        return Ok(None);
    };
    let requested_uids = cached
        .messages
        .iter()
        .map(|message| message.uid)
        .collect::<HashSet<_>>();
    let changed = match session.uid_fetch(
        cached_uid_set.clone(),
        delta_fetch_query(mode, previous_mod_seq),
    ) {
        Ok(changed) => changed,
        Err(error) => {
            tracing::debug!(folder = %folder, "IMAP incremental fetch rejected; using UID fallback: {error}");
            return Ok(None);
        }
    };
    let mut changed_flags = Vec::new();
    let mut header_uids = Vec::new();
    for item in changed.iter() {
        let Some(uid) = item.uid else { continue };
        if !requested_uids.contains(&uid) {
            tracing::debug!(folder = %folder, "IMAP incremental fetch returned an unrequested UID; using UID fallback");
            return Ok(None);
        }
        let unread = !item.flags().iter().any(|flag| matches!(flag, Flag::Seen));
        let starred = item
            .flags()
            .iter()
            .any(|flag| matches!(flag, Flag::Flagged));
        changed_flags.push((uid, unread, starred));
        if cached.messages.iter().all(|message| message.uid != uid) {
            header_uids.push(uid);
        }
    }

    let mut vanished_ranges = Vec::new();
    for response in session.take_all_unsolicited() {
        if let UnsolicitedResponse::Vanished { uids, .. } = response {
            vanished_ranges.extend(uids);
        }
    }
    if vanished_ranges.len() > MAX_DELTA_VANISHED_RANGES {
        tracing::debug!(folder = %folder, "IMAP vanished delta is too large; using UID fallback");
        return Ok(None);
    }

    let mut current_uids = None;
    if mode == IncrementalMode::Condstore {
        let uids = match session.uid_search(format!("UID {cached_uid_set}")) {
            Ok(uids) => uids,
            Err(error) => {
                tracing::debug!(folder = %folder, "IMAP UID existence check failed; using UID fallback: {error}");
                return Ok(None);
            }
        };
        if uids.len() > requested_uids.len() || uids.iter().any(|uid| !requested_uids.contains(uid))
        {
            tracing::debug!(folder = %folder, "IMAP UID existence check returned an unrequested UID; using UID fallback");
            return Ok(None);
        }
        current_uids = Some(uids.into_iter().collect::<HashSet<_>>());
    }

    let max_cached_uid = cached
        .messages
        .iter()
        .map(|message| message.uid)
        .max()
        .unwrap_or(0);
    if max_cached_uid == u32::MAX {
        return Ok(None);
    }
    let new_uid_floor = max_cached_uid + 1;
    if highest_possible_uid.saturating_sub(new_uid_floor) >= MAX_UID_SEARCH_WINDOW {
        tracing::debug!(folder = %folder, "IMAP new-UID window is too large; rebasing through bounded UID fallback");
        return Ok(None);
    }
    let new_search = match search_uid_window(
        session,
        new_uid_floor,
        highest_possible_uid,
        None,
        MAX_DELTA_HEADER_UIDS + 1,
        UidSearchOrder::Ascending,
        &format!("search incremental UIDs in {folder}"),
    ) {
        Ok(search) => search,
        Err(error) => {
            tracing::debug!(folder = %folder, "IMAP new-UID delta rejected; using UID fallback: {error}");
            return Ok(None);
        }
    };
    let new_uids_complete = !new_search.truncated && new_search.uids.len() <= MAX_DELTA_HEADER_UIDS;
    header_uids.extend(new_search.uids);
    header_uids.sort_unstable();
    header_uids.dedup();
    let delta_headers_complete = new_uids_complete && header_uids.len() <= MAX_DELTA_HEADER_UIDS;
    header_uids.truncate(MAX_DELTA_HEADER_UIDS);

    let fetched_messages = match fetch_header_messages(
        session,
        account_id,
        folder,
        &header_uids,
        &format!("fetch {folder} incremental message headers"),
    ) {
        Ok(messages) => messages,
        Err(error) => {
            tracing::debug!(folder = %folder, "IMAP incremental header fetch failed; using UID fallback: {error}");
            return Ok(None);
        }
    };

    let fetched_uids = fetched_messages
        .iter()
        .map(|message| message.uid)
        .collect::<HashSet<_>>();
    let all_requested_headers_fetched = header_uids.iter().all(|uid| fetched_uids.contains(uid));
    let mut delta = MailboxSyncDelta::default();
    if let Some(current_uids) = current_uids {
        delta.removed_uids.extend(
            cached
                .messages
                .iter()
                .filter(|message| !current_uids.contains(&message.uid))
                .map(|message| message.uid),
        );
        cached
            .messages
            .retain(|message| current_uids.contains(&message.uid));
    } else if !vanished_ranges.is_empty() {
        delta.removed_uids.extend(
            cached
                .messages
                .iter()
                .filter(|message| {
                    vanished_ranges
                        .iter()
                        .any(|range| range.contains(&message.uid))
                })
                .map(|message| message.uid),
        );
        cached.messages.retain(|message| {
            !vanished_ranges
                .iter()
                .any(|range| range.contains(&message.uid))
        });
    }
    for (uid, unread, starred) in changed_flags {
        if let Some(message) = cached
            .messages
            .iter_mut()
            .find(|message| message.uid == uid)
        {
            if message.unread != unread || message.starred != starred {
                message.unread = unread;
                message.starred = starred;
                delta.flag_updates.push((uid, unread, starred));
            }
        }
    }
    cached
        .messages
        .retain(|message| !fetched_messages.iter().any(|item| item.uid == message.uid));
    cached.messages.extend(fetched_messages.iter().cloned());
    cached
        .messages
        .sort_by_key(|message| std::cmp::Reverse(message.uid));
    cached.uid_validity = uid_validity;
    cached.highest_mod_seq = if delta_headers_complete && all_requested_headers_fetched {
        Some(current_mod_seq)
    } else {
        Some(previous_mod_seq)
    };
    cached.synced_at = synced_at;
    cached.oldest_uid = cached.messages.iter().map(|message| message.uid).min();
    delta.new_messages = fetched_messages;
    Ok(Some(delta))
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
    let provider = profile.provider;
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
                let Some(delay) = retry_delay(&error, provider, attempt) else {
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
    let cached_state = load_mailbox_for_sync(cache_root, account_id, folder)?;
    let mut force_full_rewrite = cached_state.is_none();
    let mut cached = cached_state.unwrap_or_else(|| CachedMailbox::empty(account_id, folder));
    let uid_validity_changed =
        cached.uid_validity.is_some() && cached.uid_validity != mailbox.uid_validity;
    if uid_validity_changed {
        force_full_rewrite = true;
        cached.messages.clear();
        cached.oldest_uid = None;
        cached.has_more = false;
        cached.highest_mod_seq = None;
    }
    let effective_before_uid = if uid_validity_changed {
        None
    } else {
        before_uid
    };
    let highest_possible_uid = match effective_before_uid {
        Some(uid) if uid <= 1 => {
            let unread = if folder.eq_ignore_ascii_case("INBOX") {
                cached
                    .messages
                    .iter()
                    .filter(|message| message.unread)
                    .count()
            } else {
                load_mailbox_for_sync(cache_root, account_id, "INBOX")?
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
                folder_labels: folder_display_labels(&[folder.to_string()]),
            });
        }
        Some(uid) => uid - 1,
        None => highest_mailbox_uid(
            &mut session,
            mailbox.uid_next,
            mailbox.exists,
            &format!("probe highest UID in {folder}"),
        )?,
    };
    if highest_possible_uid == 0 {
        cached.uid_validity = mailbox.uid_validity;
        cached.highest_mod_seq = mailbox.highest_mod_seq.or(cached.highest_mod_seq);
        cached.synced_at = now_stamp();
        cached.has_more = false;
        save_mailbox(cache_root, account_id, &cached)?;
        session.logout().ok();
        return Ok(SyncResult {
            account_id: account_id.to_string(),
            folder: folder.to_string(),
            fetched: 0,
            unread: cached
                .messages
                .iter()
                .filter(|message| message.unread)
                .count(),
            cache_path: cache_root
                .join(safe_component(account_id))
                .join(cache_file_name(folder))
                .display()
                .to_string(),
            synced_at: cached.synced_at,
            folders: vec![folder.to_string()],
            folder_labels: folder_display_labels(&[folder.to_string()]),
        });
    }
    let uid_floor = highest_possible_uid
        .saturating_sub(MAX_UID_SEARCH_WINDOW.saturating_sub(1))
        .max(1);
    let search = search_uid_window(
        &mut session,
        uid_floor,
        highest_possible_uid,
        None,
        page_size,
        UidSearchOrder::Descending,
        &format!("search older messages in {folder}"),
    )?;
    let has_more = search.truncated || uid_floor > 1;
    let selected_uids = search.uids;
    let mut fetched_messages = fetch_header_messages(
        &mut session,
        account_id,
        folder,
        &selected_uids,
        &format!("fetch {folder} message headers"),
    )?;
    fetched_messages.sort_by_key(|message| std::cmp::Reverse(message.uid));

    cached.uid_validity = mailbox.uid_validity;
    cached.highest_mod_seq = mailbox.highest_mod_seq.or(cached.highest_mod_seq);
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
    let fetched = fetched_messages.len();
    let path = if force_full_rewrite {
        save_mailbox(cache_root, account_id, &cached)?
    } else {
        save_mailbox_delta(
            cache_root,
            account_id,
            &cached,
            &MailboxSyncDelta {
                new_messages: fetched_messages,
                ..MailboxSyncDelta::default()
            },
        )?
    };
    crate::cache_db::spawn_search_indexer(cache_root.to_path_buf());
    let unread = if folder.eq_ignore_ascii_case("INBOX") {
        cached
            .messages
            .iter()
            .filter(|message| message.unread)
            .count()
    } else {
        load_mailbox_for_sync(cache_root, account_id, "INBOX")?
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
        fetched,
        unread,
        cache_path: path.display().to_string(),
        synced_at: cached.synced_at,
        folders: vec![folder.to_string()],
        folder_labels: folder_display_labels(&[folder.to_string()]),
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

/// Return a privacy-safe category for diagnostics without persisting provider responses,
/// addresses, or credentials in the local application log.
pub fn error_category(error: &anyhow::Error, provider: ProviderKind) -> &'static str {
    match classify_sync_error_for_provider(error, provider) {
        SyncErrorClass::Authentication => "authentication",
        SyncErrorClass::RateLimited => "rate-limit",
        SyncErrorClass::Transport => "network",
        SyncErrorClass::Permanent => "provider",
    }
}

/// Produce a stable, privacy-safe transport fingerprint for local diagnostics. This deliberately
/// records only the Rust/IMAP error variant, never server response text or message contents.
pub fn error_detail(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        if let Some(imap_error) = cause.downcast_ref::<imap::Error>() {
            return match imap_error {
                imap::Error::Io(io_error) => match io_error.kind() {
                    std::io::ErrorKind::TimedOut => "io-timeout",
                    std::io::ErrorKind::ConnectionReset => "io-connection-reset",
                    std::io::ErrorKind::ConnectionRefused => "io-connection-refused",
                    std::io::ErrorKind::ConnectionAborted => "io-connection-aborted",
                    std::io::ErrorKind::UnexpectedEof => "io-unexpected-eof",
                    _ => "io-other",
                },
                imap::Error::TlsHandshake(_) => "tls-handshake",
                imap::Error::Tls(_) => "tls",
                imap::Error::Bad(_) => "imap-bad",
                imap::Error::No(_) => "imap-no",
                imap::Error::Bye(_) => "imap-bye",
                imap::Error::ConnectionLost => "imap-connection-lost",
                imap::Error::Parse(_) => "imap-parse",
                imap::Error::Validate(_) => "imap-validate",
                imap::Error::Append => "imap-append",
                imap::Error::Unexpected(_) => "imap-unexpected",
                imap::Error::MissingStatusResponse => "imap-missing-status",
                imap::Error::TagMismatch(_) => "imap-tag-mismatch",
                imap::Error::StartTlsNotAvailable => "imap-starttls-unavailable",
                imap::Error::TlsNotConfigured => "imap-tls-not-configured",
                _ => "imap-other",
            };
        }
    }
    "other"
}

/// Only transient provider failures may enter the encrypted offline mutation queue. Permanent
/// command failures and authentication errors must reach the renderer immediately instead of
/// being replayed forever with stale credentials or an invalid destination.
pub fn is_retryable_error(error: &anyhow::Error, provider: ProviderKind) -> bool {
    matches!(
        classify_sync_error_for_provider(error, provider),
        SyncErrorClass::RateLimited | SyncErrorClass::Transport
    )
}

fn classify_sync_error(error: &anyhow::Error) -> SyncErrorClass {
    let message = error_chain_text(error);
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
        "connection limit exceeded",
        "throttl",
        "quota exceeded",
        "server busy",
        "system busy",
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

fn classify_sync_error_for_provider(
    error: &anyhow::Error,
    provider: ProviderKind,
) -> SyncErrorClass {
    let generic = classify_sync_error(error);
    if generic != SyncErrorClass::Permanent {
        return generic;
    }
    let message = error_chain_text(error);
    let provider_markers: &[&str] = match provider {
        ProviderKind::Google => &[
            "[limit]",
            "user-rate limit exceeded",
            "bandwidth limit exceeded",
            "maximum number of connections",
            "too many simultaneous connections",
        ],
        ProviderKind::Outlook => &[
            "mailbox is throttled",
            "maximum number of connections",
            "resource temporarily unavailable",
        ],
        ProviderKind::Qq | ProviderKind::Other => &[],
    };
    if provider_markers
        .iter()
        .any(|marker| message.contains(marker))
    {
        SyncErrorClass::RateLimited
    } else {
        SyncErrorClass::Permanent
    }
}

fn retry_delay(error: &anyhow::Error, provider: ProviderKind, attempt: u32) -> Option<Duration> {
    if let Some(retry_after) = retry_after_seconds(error) {
        return Some(Duration::from_secs(retry_after.clamp(1, 300)));
    }
    let delay_seconds = match classify_sync_error_for_provider(error, provider) {
        SyncErrorClass::Transport => 1u64 << attempt,
        SyncErrorClass::RateLimited => 5u64.saturating_mul(1u64 << attempt),
        SyncErrorClass::Authentication | SyncErrorClass::Permanent => return None,
    };
    Some(Duration::from_secs(delay_seconds.min(60)))
}

/// Retry read-only IMAP operations a small, bounded number of times. Mutating commands are
/// intentionally excluded: replaying a command after a server-side timeout could apply it twice.
fn retry_imap_operation<T, F>(
    account_id: &str,
    operation: &str,
    provider: ProviderKind,
    operation_fn: F,
) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    retry_imap_operation_with_sleeper(account_id, operation, provider, operation_fn, thread::sleep)
}

fn retry_imap_operation_with_sleeper<T, F, S>(
    account_id: &str,
    operation: &str,
    provider: ProviderKind,
    mut operation_fn: F,
    mut sleep_fn: S,
) -> Result<T>
where
    F: FnMut() -> Result<T>,
    S: FnMut(Duration),
{
    let mut attempt = 0u32;
    loop {
        match operation_fn() {
            Ok(value) => return Ok(value),
            Err(error) if attempt < 2 => {
                let Some(delay) = retry_delay(&error, provider, attempt) else {
                    return Err(error);
                };
                attempt += 1;
                tracing::warn!(
                    account_id = %account_id,
                    operation,
                    attempt,
                    delay = delay.as_secs(),
                    "recoverable IMAP read failure; retrying"
                );
                sleep_fn(delay);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Honor a provider's Retry-After hint when it survives the transport error text. The IMAP crate
/// does not expose arbitrary response headers, so this parser is intentionally conservative and
/// falls back to bounded exponential delays when no numeric hint is present.
fn retry_after_seconds(error: &anyhow::Error) -> Option<u64> {
    let message = error_chain_text(error);
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

fn error_chain_text(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|source| source.to_string())
        .collect::<Vec<_>>()
        .join(": ")
        .to_ascii_lowercase()
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
    retry_imap_operation(account_id, "full message fetch", profile.provider, || {
        fetch_message_once(
            account_id,
            profile.clone(),
            email,
            credential,
            folder,
            uid,
            cache_root,
        )
    })
}

fn fetch_message_once(
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
    folder: &str,
    uid: u32,
    cache_root: &Path,
) -> Result<MailDetail> {
    validate_mailbox_name(folder)?;
    let (raw, unread, starred) = fetch_raw_message_with_pooled_session(
        account_id, &profile, email, credential, folder, uid,
    )?;
    let (mut message, payloads) =
        parse_full_with_attachments(account_id, folder, uid, unread, starred, &raw)?;
    crate::mail::embed_inline_images(&mut message.html_body, &payloads);
    store_attachment_payloads(cache_root, account_id, folder, uid, &mut message, &payloads)?;
    Ok(MailDetail { message })
}

fn validate_advertised_message_size(size: u32) -> Result<()> {
    if size as usize > crate::mail::MAX_FULL_MESSAGE_BYTES {
        return Err(anyhow!(
            "message is larger than the {0} MiB safety limit",
            crate::mail::MAX_FULL_MESSAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok(())
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
        .join(cache_folder_component(folder));
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
    let message = load_cached_message(cache_root, account_id, folder, uid)?
        .ok_or_else(|| anyhow!("message is not cached"))?;
    let metadata = message
        .attachments
        .iter()
        .find(|attachment| attachment.index == index)
        .ok_or_else(|| anyhow!("attachment is not available"))?;
    let path = cache_root
        .join(safe_component(account_id))
        .join("attachments")
        .join(cache_folder_component(folder))
        .join(format!("{uid}-{index}.bin"));
    let encrypted = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy_path = cache_root
                .join(safe_component(account_id))
                .join("attachments")
                .join(legacy_folder_component(folder))
                .join(format!("{uid}-{index}.bin"));
            fs::read(&legacy_path)
                .with_context(|| format!("read attachment {}", legacy_path.display()))?
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read attachment {}", path.display()))
        }
    };
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
    if let Some(mailbox) = crate::cache_db::load_mailbox(cache_root, account_id, folder)? {
        return Ok(Some(mailbox));
    }
    let directory = cache_root.join(safe_component(account_id));
    let encrypted_path = directory.join(cache_file_name(folder));
    let mut candidates = vec![
        (encrypted_path.clone(), true),
        (encrypted_path.with_extension("bin.bak"), true),
    ];
    let legacy_path = directory.join(legacy_cache_file_name(folder));
    if legacy_path != encrypted_path {
        candidates.push((legacy_path.clone(), true));
        candidates.push((legacy_path.with_extension("bin.bak"), true));
    }
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
                // Legacy encrypted snapshots are migrated transactionally on first access. They
                // remain in place as a recovery source until normal cache cleanup removes them.
                crate::cache_db::save_mailbox(cache_root, account_id, &mailbox)
                    .context("migrate legacy mailbox into indexed cache")?;
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

fn load_mailbox_for_sync(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
) -> Result<Option<CachedMailbox>> {
    if let Some(mailbox) = crate::cache_db::load_mailbox_summaries(
        cache_root,
        account_id,
        folder,
        MAX_CACHED_MESSAGES_PER_FOLDER,
    )? {
        return Ok(Some(mailbox));
    }
    if load_mailbox_for_folder(cache_root, account_id, folder)?.is_none() {
        return Ok(None);
    }
    crate::cache_db::load_mailbox_summaries(
        cache_root,
        account_id,
        folder,
        MAX_CACHED_MESSAGES_PER_FOLDER,
    )
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
    clear_body_session(account_id);
    crate::cache_db::remove_account(cache_root, account_id)?;
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
    if let Some(message) = crate::cache_db::load_message(cache_root, account_id, folder, uid)? {
        return Ok(Some(message));
    }
    // A one-time legacy load populates the index; subsequent exact reads stay O(log n).
    if load_mailbox_for_folder(cache_root, account_id, folder)?.is_none() {
        return Ok(None);
    }
    crate::cache_db::load_message(cache_root, account_id, folder, uid)
}

pub fn load_mailbox_page(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    before_uid: Option<u32>,
    limit: usize,
) -> Result<Option<crate::cache_db::MailboxPage>> {
    validate_mailbox_name(folder)?;
    if let Some(page) =
        crate::cache_db::load_mailbox_page(cache_root, account_id, folder, before_uid, limit)?
    {
        return Ok(Some(page));
    }
    if load_mailbox_for_folder(cache_root, account_id, folder)?.is_none() {
        return Ok(None);
    }
    crate::cache_db::load_mailbox_page(cache_root, account_id, folder, before_uid, limit)
}

pub fn save_cached_message(
    cache_root: &Path,
    account_id: &str,
    message: &CachedMessage,
) -> Result<()> {
    crate::cache_db::save_message(cache_root, account_id, message)?;
    crate::cache_db::spawn_search_indexer(cache_root.to_path_buf());
    Ok(())
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
    if !crate::cache_db::mailbox_exists(cache_root, account_id, folder)? {
        // Preserve a not-yet-migrated encrypted snapshot before the first row-level merge creates
        // indexed metadata that would otherwise hide the legacy recovery source.
        let _ = load_mailbox_for_folder(cache_root, account_id, folder)?;
    }
    crate::cache_db::merge_messages(
        cache_root,
        account_id,
        folder,
        uid_validity,
        &now_stamp(),
        messages,
        MAX_CACHED_MESSAGES_PER_FOLDER,
    )
}

pub fn update_cached_flags(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    flag: &str,
    enabled: bool,
) -> Result<()> {
    let Some(mut message) = load_cached_message(cache_root, account_id, folder, uid)? else {
        return Ok(());
    };
    match flag {
        "\\Seen" => message.unread = !enabled,
        "\\Flagged" => message.starred = enabled,
        _ => return Ok(()),
    }
    crate::cache_db::save_message(cache_root, account_id, &message)
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
    let moved = crate::cache_db::move_message(
        cache_root,
        account_id,
        folder,
        uid,
        target_folder,
        &now_stamp(),
        MAX_CACHED_MESSAGES_PER_FOLDER,
    )?;
    if moved {
        crate::cache_db::spawn_search_indexer(cache_root.to_path_buf());
        return Ok(());
    }
    if crate::cache_db::mailbox_exists(cache_root, account_id, folder)? {
        return Ok(());
    }
    // A one-time legacy migration keeps optimistic move behavior compatible with pre-index
    // installations. Current indexed mailboxes never take this full-snapshot fallback.
    if load_mailbox_for_folder(cache_root, account_id, folder)?.is_some() {
        let moved = crate::cache_db::move_message(
            cache_root,
            account_id,
            folder,
            uid,
            target_folder,
            &now_stamp(),
            MAX_CACHED_MESSAGES_PER_FOLDER,
        )?;
        if moved {
            crate::cache_db::spawn_search_indexer(cache_root.to_path_buf());
        }
    }
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
    crate::cache_db::remove_message(cache_root, account_id, folder, uid)
}

fn validate_mailbox_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 512 || name.chars().any(char::is_control) {
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

pub fn is_spam_folder(provider: crate::providers::ProviderKind, folder: &str) -> bool {
    let expected = match provider {
        crate::providers::ProviderKind::Google => "[Gmail]/Spam",
        crate::providers::ProviderKind::Qq => "Spam",
        crate::providers::ProviderKind::Outlook => "Junk Email",
        crate::providers::ProviderKind::Other => "Spam",
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

fn save_mailbox_delta(
    cache_root: &Path,
    account_id: &str,
    mailbox: &CachedMailbox,
    delta: &MailboxSyncDelta,
) -> Result<PathBuf> {
    let _write_guard = cache_write_guard();
    crate::cache_db::apply_mailbox_sync_delta(
        cache_root,
        account_id,
        mailbox,
        &delta.new_messages,
        &delta.flag_updates,
        &delta.removed_uids,
        MAX_CACHED_MESSAGES_PER_FOLDER,
    )
}

fn save_mailbox_unlocked(
    cache_root: &Path,
    account_id: &str,
    mailbox: &CachedMailbox,
) -> Result<PathBuf> {
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
    crate::cache_db::save_mailbox(cache_root, account_id, &bounded_mailbox)
}

fn cache_encryption_key() -> Result<[u8; CACHE_ENCRYPTION_KEY_BYTES]> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use zeroize::Zeroize;

    let cache = CACHE_ENCRYPTION_KEY.get_or_init(|| Mutex::new(None));
    let mut cached = cache
        .lock()
        .map_err(|_| anyhow!("protected cache key lock poisoned"))?;
    if let Some(key) = *cached {
        return Ok(key);
    }

    let entry = keyring::Entry::new(CACHE_ENCRYPTION_KEY_SERVICE, CACHE_ENCRYPTION_KEY_ACCOUNT)
        .map_err(|error| anyhow!("protected cache keyring unavailable: {error}"))?;
    let key = match entry.get_password() {
        Ok(mut encoded) => {
            let decoded = STANDARD.decode(&encoded);
            encoded.zeroize();
            let mut decoded =
                decoded.map_err(|_| anyhow!("protected cache keyring contains an invalid key"))?;
            let key = decoded
                .as_slice()
                .try_into()
                .map_err(|_| anyhow!("protected cache keyring contains an invalid key length"))?;
            decoded.zeroize();
            key
        }
        Err(keyring::Error::NoEntry) => {
            let mut key = [0_u8; CACHE_ENCRYPTION_KEY_BYTES];
            let mut rng = rand::thread_rng();
            rand::RngCore::fill_bytes(&mut rng, &mut key);
            let mut encoded = STANDARD.encode(key);
            let result = entry
                .set_password(&encoded)
                .map_err(|error| anyhow!("save protected cache key: {error}"));
            encoded.zeroize();
            result?;
            key
        }
        Err(error) => return Err(anyhow!("load protected cache key: {error}")),
    };
    *cached = Some(key);
    Ok(key)
}

pub(crate) fn database_cache_uses_current_envelope(payload: &[u8]) -> bool {
    payload.starts_with(CACHE_ENVELOPE_MAGIC)
}

fn protect_cache_envelope(payload: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

    let key = cache_encryption_key()?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    let mut nonce = [0_u8; 24];
    let mut rng = rand::thread_rng();
    rand::RngCore::fill_bytes(&mut rng, &mut nonce);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), payload)
        .map_err(|_| anyhow!("encrypt local cache"))?;
    let mut output = Vec::with_capacity(
        CACHE_ENVELOPE_MAGIC
            .len()
            .saturating_add(nonce.len())
            .saturating_add(ciphertext.len()),
    );
    output.extend_from_slice(CACHE_ENVELOPE_MAGIC);
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&ciphertext);
    Ok(output)
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

fn unprotect_cache_envelope(payload: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

    let minimum = CACHE_ENVELOPE_MAGIC
        .len()
        .saturating_add(24)
        .saturating_add(16);
    if payload.len() < minimum || !database_cache_uses_current_envelope(payload) {
        return Err(anyhow!("protected cache envelope is invalid"));
    }
    let key = cache_encryption_key()?;
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
    cipher
        .decrypt(
            XNonce::from_slice(
                &payload[CACHE_ENVELOPE_MAGIC.len()..CACHE_ENVELOPE_MAGIC.len() + 24],
            ),
            &payload[CACHE_ENVELOPE_MAGIC.len() + 24..],
        )
        .map_err(|_| anyhow!("protected cache could not be unlocked"))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn protect_cache(payload: &[u8]) -> Result<Vec<u8>> {
    protect_cache_envelope(payload)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn unprotect_cache(payload: &[u8]) -> Result<Vec<u8>> {
    unprotect_cache_envelope(payload)
}

pub(crate) fn protect_database_cache(payload: &[u8]) -> Result<Vec<u8>> {
    protect_cache_envelope(payload)
}

pub(crate) fn unprotect_database_cache(payload: &[u8]) -> Result<Vec<u8>> {
    if database_cache_uses_current_envelope(payload) {
        unprotect_cache_envelope(payload)
    } else {
        unprotect_cache(payload)
    }
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

fn hashed_component(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// Folder cache paths use a content-addressed key. IMAP folder names can contain arbitrary
/// Unicode and punctuation, so lossy replacement is not sufficient: two distinct names must
/// never share a cache file or attachment directory.
fn cache_folder_component(folder: &str) -> String {
    if folder.eq_ignore_ascii_case("INBOX") {
        "INBOX".into()
    } else {
        format!("folder-{}", hashed_component(folder))
    }
}

fn legacy_folder_component(folder: &str) -> String {
    safe_component(folder)
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
        format!("folder_{}.bin", hashed_component(folder))
    }
}

fn legacy_cache_file_name(folder: &str) -> String {
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
            let mut bounded = Vec::with_capacity(MAX_DISCOVERED_FOLDERS);
            for name in names.iter().map(|name| name.name().to_string()) {
                if !is_safe_discovered_folder(&name) {
                    continue;
                }
                let is_preferred = preferred
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate));
                if is_preferred || bounded.len() < MAX_DISCOVERED_FOLDERS {
                    bounded.push(name);
                }
            }
            bounded
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

fn reset_mailbox_sync_window(mailbox: &mut CachedMailbox) {
    mailbox.messages.clear();
    mailbox.oldest_uid = None;
    mailbox.has_more = false;
    mailbox.highest_mod_seq = None;
}

fn upgrade_mailbox_cache(mailbox: &mut CachedMailbox) -> bool {
    if mailbox.schema_version >= CACHE_SCHEMA_VERSION {
        return false;
    }
    mailbox.schema_version = CACHE_SCHEMA_VERSION;
    reset_mailbox_sync_window(mailbox);
    true
}

pub(crate) fn folder_display_name(name: &str) -> String {
    if !name.contains('&') {
        return name.to_string();
    }

    let mut output = String::with_capacity(name.len());
    let mut cursor = 0usize;
    while cursor < name.len() {
        let Some(relative_start) = name[cursor..].find('&') else {
            output.push_str(&name[cursor..]);
            break;
        };
        let start = cursor + relative_start;
        output.push_str(&name[cursor..start]);
        let encoded_start = start + 1;
        let Some(relative_end) = name[encoded_start..].find('-') else {
            output.push_str(&name[start..]);
            break;
        };
        let end = encoded_start + relative_end;
        if end == encoded_start {
            output.push('&');
            cursor = end + 1;
            continue;
        }

        let original = &name[start..=end];
        let encoded = &name[encoded_start..end];
        let engine = GeneralPurpose::new(&IMAP_MUTF7, NO_PAD);
        let decoded = engine.decode(encoded.as_bytes()).ok().and_then(|bytes| {
            let (pairs, remainder) = bytes.as_chunks::<2>();
            if !remainder.is_empty() {
                return None;
            }
            let words = pairs
                .iter()
                .map(|pair| u16::from_be_bytes(*pair))
                .collect::<Vec<_>>();
            String::from_utf16(&words)
                .ok()
                .filter(|value| !value.chars().any(char::is_control))
        });
        output.push_str(decoded.as_deref().unwrap_or(original));
        cursor = end + 1;
    }
    output
}

fn folder_display_labels(folders: &[String]) -> HashMap<String, String> {
    folders
        .iter()
        .map(|folder| (folder.clone(), folder_display_name(folder)))
        .collect()
}

fn is_safe_discovered_folder(name: &str) -> bool {
    name.trim().len() <= 512 && !name.trim().is_empty() && !name.chars().any(char::is_control)
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
    fn starttls_plaintext_line_is_rejected_before_unbounded_allocation() {
        let mut reader = BufReader::new(std::io::Cursor::new(vec![
            b'a';
            MAX_STARTTLS_LINE_BYTES + 1
        ]));
        let mut line = Vec::new();
        let error = read_bounded_starttls_line(&mut reader, &mut line, |_| Ok(()))
            .expect_err("an unterminated oversized line must be rejected");

        assert!(error.to_string().contains("line is too large"));
        assert!(line.len() <= MAX_STARTTLS_LINE_BYTES);
    }

    #[test]
    fn starttls_plaintext_line_accepts_a_bounded_complete_response() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"MAILGO1 OK Begin TLS\r\n"));
        let mut line = Vec::new();
        let read = read_bounded_starttls_line(&mut reader, &mut line, |_| Ok(())).unwrap();

        assert_eq!(read, line.len());
        assert_eq!(line, b"MAILGO1 OK Begin TLS\r\n");
    }

    #[test]
    fn bounded_worker_pool_preserves_order_and_caps_parallelism() {
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let values = (0..24).collect::<Vec<_>>();
        let results = map_with_concurrency(&values, 3, |value| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(2));
            active.fetch_sub(1, Ordering::SeqCst);
            value * 2
        });
        assert_eq!(
            results,
            values.iter().map(|value| value * 2).collect::<Vec<_>>()
        );
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert!(peak.load(Ordering::SeqCst) > 1);
    }

    fn fixture_imap_session<F>(
        scenario: F,
    ) -> (imap::Session<imap::Connection>, thread::JoinHandle<()>)
    where
        F: FnOnce(&mut BufReader<TcpStream>, &mut TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind IMAP fixture");
        let address = listener.local_addr().expect("IMAP fixture address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept IMAP fixture");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set IMAP fixture read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("set IMAP fixture write timeout");
            stream
                .write_all(b"* OK MailGo provider fixture ready\r\n")
                .expect("write IMAP fixture greeting");
            stream.flush().expect("flush IMAP fixture greeting");
            let mut reader = BufReader::new(stream.try_clone().expect("clone IMAP fixture"));
            let mut command = String::new();
            reader
                .read_line(&mut command)
                .expect("read IMAP fixture LOGIN");
            assert!(command.to_ascii_uppercase().contains(" LOGIN "));
            let tag = command.split_ascii_whitespace().next().expect("LOGIN tag");
            write!(stream, "{tag} OK LOGIN completed\r\n")
                .expect("write IMAP fixture LOGIN response");
            stream.flush().expect("flush IMAP fixture LOGIN response");
            scenario(&mut reader, &mut stream);
        });

        let stream = TcpStream::connect(address).expect("connect IMAP fixture");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture client read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture client write timeout");
        let mut client = imap::Client::new(Box::new(stream) as imap::Connection);
        client.read_greeting().expect("read IMAP fixture greeting");
        let session = client
            .login("fixture@example.invalid", "fixture-password")
            .map_err(|(error, _)| error)
            .expect("login to IMAP fixture");
        (session, server)
    }

    fn respond_to_select(
        reader: &mut BufReader<TcpStream>,
        stream: &mut TcpStream,
        uid_validity: u32,
    ) {
        let mut command = String::new();
        reader
            .read_line(&mut command)
            .expect("read IMAP fixture SELECT");
        assert!(command.to_ascii_uppercase().contains(" SELECT "));
        let tag = command.split_ascii_whitespace().next().expect("SELECT tag");
        write!(
            stream,
            "* FLAGS (\\Seen \\Flagged)\r\n* 1 EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY {uid_validity}] UIDs valid\r\n{tag} OK [READ-WRITE] SELECT completed\r\n"
        )
        .expect("write IMAP fixture SELECT response");
        stream.flush().expect("flush IMAP fixture SELECT response");
    }

    #[test]
    fn cache_envelope_is_randomized_authenticated_and_round_trips() {
        let plaintext = b"mailgo protected cache fixture";
        let first = protect_database_cache(plaintext).unwrap();
        let second = protect_database_cache(plaintext).unwrap();
        assert!(database_cache_uses_current_envelope(&first));
        assert!(database_cache_uses_current_envelope(&second));
        assert_ne!(first, second);
        assert!(!first
            .windows(plaintext.len())
            .any(|window| window == plaintext));
        assert_eq!(unprotect_database_cache(&first).unwrap(), plaintext);

        let last = first.len() - 1;
        let mut tampered = first;
        tampered[last] ^= 1;
        assert!(unprotect_database_cache(&tampered).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn current_reader_accepts_legacy_windows_dpapi_payloads() {
        let plaintext = b"legacy Windows cache fixture";
        let encrypted = protect_cache(plaintext).unwrap();
        assert!(!database_cache_uses_current_envelope(&encrypted));
        assert_eq!(unprotect_database_cache(&encrypted).unwrap(), plaintext);
    }

    #[test]
    fn imap_socket_applies_connect_and_io_timeouts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fixture socket");
        let port = listener.local_addr().expect("fixture address").port();
        let stream = connect_socket("127.0.0.1", port).expect("connect fixture socket");
        assert_eq!(stream.read_timeout().unwrap(), Some(IMAP_IO_TIMEOUT));
        assert_eq!(stream.write_timeout().unwrap(), Some(IMAP_IO_TIMEOUT));
    }

    #[test]
    fn bounded_imap_stream_never_allows_idle_to_clear_the_io_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind timeout fixture");
        let stream = TcpStream::connect(listener.local_addr().unwrap()).expect("connect fixture");
        let mut bounded = BoundedImapStream::new(stream);
        SetReadTimeout::set_read_timeout(&mut bounded, None).expect("retain timeout");
        assert_eq!(bounded.inner.read_timeout().unwrap(), Some(IMAP_IO_TIMEOUT));
        let shorter = Duration::from_secs(2);
        SetReadTimeout::set_read_timeout(&mut bounded, Some(shorter)).expect("set shorter timeout");
        assert_eq!(bounded.inner.read_timeout().unwrap(), Some(shorter));
    }

    #[test]
    fn provider_spam_folder_mapping_is_case_insensitive_and_provider_specific() {
        assert!(is_spam_folder(
            crate::providers::ProviderKind::Google,
            "[gmail]/spam"
        ));
        assert!(is_spam_folder(crate::providers::ProviderKind::Qq, "spam"));
        assert!(is_spam_folder(
            crate::providers::ProviderKind::Outlook,
            "JUNK EMAIL"
        ));
        assert!(!is_spam_folder(
            crate::providers::ProviderKind::Google,
            "[Gmail]/Trash"
        ));
    }

    #[test]
    fn provider_folder_fixtures_cover_google_qq_and_outlook_system_mailboxes() {
        let google = folders_for(ProviderKind::Google);
        assert!(google.contains(&"[Gmail]/All Mail"));
        assert!(google.contains(&"[Gmail]/Trash"));

        let qq = folders_for(ProviderKind::Qq);
        assert!(qq.contains(&"Sent Messages"));
        assert!(qq.contains(&"Spam"));

        let outlook = folders_for(ProviderKind::Outlook);
        assert!(outlook.contains(&"Sent Items"));
        assert!(outlook.contains(&"Junk Email"));
        assert!(outlook.contains(&"Deleted Items"));
    }

    #[test]
    fn starttls_fixture_accepts_only_the_fixed_tagged_ok_response() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind STARTTLS fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept STARTTLS fixture");
            stream
                .write_all(b"* OK fixture ready\r\n")
                .expect("write STARTTLS greeting");
            stream.flush().expect("flush STARTTLS greeting");
            let mut request = [0_u8; 18];
            std::io::Read::read_exact(&mut stream, &mut request).expect("read STARTTLS command");
            assert_eq!(&request, b"MAILGO1 STARTTLS\r\n");
            std::io::Write::write_all(
                &mut stream,
                b"* OK ready\r\nMAILGO1X OK prefix tags must not match\r\nMAILGO1 OK Begin TLS negotiation now\r\n",
            )
            .expect("write STARTTLS response");
        });
        let tcp = TcpStream::connect(address).expect("connect STARTTLS fixture");
        tcp.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set fixture read timeout");
        start_tls(tcp).expect("STARTTLS fixture should be accepted");
        server.join().expect("join STARTTLS fixture");
    }

    #[test]
    fn idle_fixture_wakes_on_an_exists_response() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind IDLE fixture");
        let address = listener.local_addr().expect("IDLE fixture address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept IDLE fixture");
            stream
                .write_all(b"* OK fixture ready\r\n")
                .expect("write IMAP greeting");
            stream.flush().expect("flush IMAP greeting");
            let mut reader = BufReader::new(stream.try_clone().expect("clone IDLE fixture"));

            let mut command = String::new();
            reader.read_line(&mut command).expect("read LOGIN command");
            let login_tag = command.split_ascii_whitespace().next().unwrap();
            write!(stream, "{login_tag} OK LOGIN completed\r\n").expect("write LOGIN response");
            stream.flush().expect("flush LOGIN response");

            command.clear();
            reader
                .read_line(&mut command)
                .expect("read CAPABILITY command");
            let capability_tag = command.split_ascii_whitespace().next().unwrap();
            write!(
                stream,
                "* CAPABILITY IMAP4rev1 IDLE\r\n{capability_tag} OK CAPABILITY completed\r\n"
            )
            .expect("write CAPABILITY response");
            stream.flush().expect("flush CAPABILITY response");

            command.clear();
            reader.read_line(&mut command).expect("read SELECT command");
            let select_tag = command.split_ascii_whitespace().next().unwrap();
            write!(
                stream,
                "* FLAGS (\\Seen)\r\n* 0 EXISTS\r\n{select_tag} OK [READ-WRITE] SELECT completed\r\n"
            )
            .expect("write SELECT response");
            stream.flush().expect("flush SELECT response");

            command.clear();
            reader.read_line(&mut command).expect("read IDLE command");
            assert!(command.to_ascii_uppercase().contains(" IDLE"));
            let idle_tag = command.split_ascii_whitespace().next().unwrap().to_string();
            stream
                .write_all(b"+ idling\r\n* 1 EXISTS\r\n")
                .expect("write IDLE update");
            stream.flush().expect("flush IDLE update");

            command.clear();
            reader.read_line(&mut command).expect("read IDLE DONE");
            assert_eq!(command, "DONE\r\n");
            write!(stream, "{idle_tag} OK IDLE terminated\r\n").expect("write IDLE completion");
            stream.flush().expect("flush IDLE completion");
        });

        let stream = TcpStream::connect(address).expect("connect IDLE fixture");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set IDLE fixture read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set IDLE fixture write timeout");
        let mut client = imap::Client::new(stream);
        client.read_greeting().expect("read IDLE fixture greeting");
        let mut session = client
            .login("fixture@example.invalid", "fixture-password")
            .map_err(|(error, _)| error)
            .expect("login to IDLE fixture");
        assert!(prepare_idle_session(&mut session).expect("prepare IDLE fixture"));
        assert_eq!(
            wait_for_idle_signal(&mut session, Duration::from_secs(2))
                .expect("wait for IDLE fixture update"),
            WaitOutcome::MailboxChanged
        );
        drop(session);
        server.join().expect("join IDLE fixture");
    }

    #[test]
    fn idle_policy_pauses_for_offline_mode_and_transitional_accounts() {
        assert!(idle_watch_enabled(false, "synced"));
        assert!(idle_watch_enabled(false, "offline"));
        assert!(!idle_watch_enabled(true, "synced"));
        assert!(!idle_watch_enabled(false, "needs-auth"));
        assert!(!idle_watch_enabled(false, "syncing"));
        assert!(IDLE_STATE_CHECK_INTERVAL < Duration::from_secs(29 * 60));
    }

    #[test]
    fn idle_reconnect_backoff_is_bounded() {
        assert_eq!(idle_retry_delay(0), Duration::from_secs(15));
        assert_eq!(idle_retry_delay(1), Duration::from_secs(30));
        assert_eq!(idle_retry_delay(4), Duration::from_secs(240));
        assert_eq!(idle_retry_delay(5), Duration::from_secs(300));
        assert_eq!(idle_retry_delay(50), Duration::from_secs(300));
    }

    fn fixture_message(uid: u32, folder: &str) -> CachedMessage {
        CachedMessage {
            id: format!("fixture-{uid}@example.invalid"),
            message_id: Some(format!("fixture-{uid}@example.invalid")),
            in_reply_to: None,
            references: Vec::new(),
            thread_id: format!("fixture-{uid}@example.invalid"),
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
    fn folder_cache_keys_do_not_collide_after_normalization() {
        assert_ne!(
            cache_file_name("Projects/2026"),
            cache_file_name("Projects\\2026")
        );
        assert_ne!(
            cache_file_name("团队收件箱"),
            cache_file_name("团队_收件箱")
        );
        assert_ne!(
            cache_folder_component("Projects/2026"),
            cache_folder_component("Projects\\2026")
        );
        assert_eq!(cache_file_name("INBOX"), cache_file_name("inbox"));
        assert!(cache_file_name("Projects/2026").starts_with("folder_"));
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
    fn body_session_fingerprint_rotates_when_credentials_change() {
        let profile = crate::providers::profile_for(ProviderKind::Qq).unwrap();
        let first = body_session_fingerprint(&profile, "person@example.invalid", "secret-one");
        let same = body_session_fingerprint(&profile, "PERSON@example.invalid", "secret-one");
        let rotated = body_session_fingerprint(&profile, "person@example.invalid", "secret-two");
        assert_eq!(first, same);
        assert_ne!(first, rotated);
    }

    #[test]
    fn sync_retries_transport_and_rate_limits_but_not_auth_failures() {
        assert_eq!(
            retry_delay(&anyhow!("connect IMAP host timed out"), ProviderKind::Qq, 0),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay(
                &anyhow!("IMAP provider rate limit: too many requests"),
                ProviderKind::Google,
                1
            ),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            retry_delay(
                &anyhow!("HTTP 429 Retry-After: 37"),
                ProviderKind::Outlook,
                0
            ),
            Some(Duration::from_secs(37))
        );
        assert_eq!(
            retry_delay(&anyhow!("retry_after=9999"), ProviderKind::Other, 0),
            Some(Duration::from_secs(300))
        );
        assert!(retry_delay(
            &anyhow!("IMAP authentication failed"),
            ProviderKind::Google,
            0
        )
        .is_none());
        assert!(retry_delay(
            &anyhow!("unsupported mail provider"),
            ProviderKind::Other,
            0
        )
        .is_none());
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
        assert_eq!(
            error_category(&anyhow!("IMAP authentication failed"), ProviderKind::Qq),
            "authentication"
        );
        assert_eq!(
            error_category(&anyhow!("connection reset by peer"), ProviderKind::Qq),
            "network"
        );
        assert_eq!(
            error_detail(&anyhow::Error::new(imap::Error::ConnectionLost).context("sync")),
            "imap-connection-lost"
        );
        assert!(needs_reauthorization(&anyhow!(
            "OAuth access token expired; reauthorization is required"
        )));
        assert!(!needs_reauthorization(&anyhow!("connection reset by peer")));
        assert!(is_retryable_error(
            &anyhow!("connection reset by peer"),
            ProviderKind::Qq
        ));
        assert!(is_retryable_error(
            &anyhow!("IMAP rate limit: try again later"),
            ProviderKind::Google
        ));
        assert!(!is_retryable_error(
            &anyhow!("IMAP authentication failed"),
            ProviderKind::Google
        ));
        assert!(!is_retryable_error(
            &anyhow!("mailbox does not exist"),
            ProviderKind::Other
        ));
    }

    #[test]
    fn read_only_imap_retry_control_flow_reconnects_twice_with_bounded_backoff() {
        let mut attempts = 0usize;
        let mut delays = Vec::new();
        let value = retry_imap_operation_with_sleeper(
            "fixture-account",
            "fixture fetch",
            ProviderKind::Qq,
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(anyhow!("connection reset by peer"))
                } else {
                    Ok(42)
                }
            },
            |delay| delays.push(delay),
        )
        .unwrap();

        assert_eq!(value, 42);
        assert_eq!(attempts, 3);
        assert_eq!(delays, vec![Duration::from_secs(1), Duration::from_secs(2)]);
    }

    #[test]
    fn provider_specific_imap_throttling_is_bounded_and_not_overgeneralized() {
        assert_eq!(
            classify_sync_error_for_provider(
                &anyhow!("NO [LIMIT] Bandwidth limit exceeded").context("select INBOX"),
                ProviderKind::Google,
            ),
            SyncErrorClass::RateLimited
        );
        assert_eq!(
            classify_sync_error_for_provider(
                &anyhow!("maximum number of connections exceeded"),
                ProviderKind::Outlook,
            ),
            SyncErrorClass::RateLimited
        );
        assert_eq!(
            classify_sync_error_for_provider(
                &anyhow!("maximum number of connections exceeded"),
                ProviderKind::Qq,
            ),
            SyncErrorClass::Permanent
        );
        assert_eq!(
            retry_delay(
                &anyhow!("NO [LIMIT] user-rate limit exceeded"),
                ProviderKind::Google,
                9,
            ),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            classify_sync_error_for_provider(
                &anyhow!("mailbox does not exist"),
                ProviderKind::Outlook,
            ),
            SyncErrorClass::Permanent
        );
    }

    #[test]
    fn provider_throttling_fixtures_drive_mutations_into_the_retry_queue() {
        let fixtures = [
            (
                ProviderKind::Google,
                "NO [LIMIT] User-rate limit exceeded. (Failure)",
                true,
            ),
            (
                ProviderKind::Google,
                "NO [LIMIT] Bandwidth limit exceeded.",
                true,
            ),
            (
                ProviderKind::Google,
                "NO [ALERT] Too many simultaneous connections. (Failure)",
                true,
            ),
            (
                ProviderKind::Outlook,
                "NO Maximum number of connections from user+IP exceeded",
                true,
            ),
            (
                ProviderKind::Outlook,
                "NO Mailbox is throttled. Retry-After: 45",
                true,
            ),
            (
                ProviderKind::Qq,
                "NO System busy, please try again later",
                true,
            ),
            (ProviderKind::Qq, "NO Authentication failed", false),
            (ProviderKind::Outlook, "NO Mailbox does not exist", false),
        ];

        for (provider, response, retryable) in fixtures {
            let error = anyhow!(response).context("apply mail mutation");
            assert_eq!(
                is_retryable_error(&error, provider),
                retryable,
                "unexpected retry decision for {} fixture: {response}",
                provider.as_str()
            );
        }
    }

    #[test]
    fn advertised_message_size_is_rejected_before_full_fetch() {
        assert!(validate_advertised_message_size(1024).is_ok());
        assert!(
            validate_advertised_message_size((crate::mail::MAX_FULL_MESSAGE_BYTES + 1) as u32)
                .is_err()
        );
    }

    #[test]
    fn incremental_fetch_query_adds_vanished_only_for_qresync() {
        assert_eq!(
            delta_fetch_query(IncrementalMode::Condstore, 42),
            "(UID FLAGS MODSEQ) (CHANGEDSINCE 42)"
        );
        assert_eq!(
            delta_fetch_query(IncrementalMode::Qresync, 42),
            "(UID FLAGS MODSEQ) (CHANGEDSINCE 42 VANISHED)"
        );
    }

    #[test]
    fn multi_item_fetch_queries_are_parenthesized_for_strict_imap_servers() {
        for query in [
            HEADER_FETCH_QUERY,
            FLAGS_FETCH_QUERY,
            SIZE_FETCH_QUERY,
            FULL_FETCH_QUERY,
        ] {
            assert!(
                query.starts_with('('),
                "missing opening parenthesis: {query}"
            );
            assert!(query.ends_with(')'), "missing closing parenthesis: {query}");
            assert!(query.split_ascii_whitespace().count() >= 2);
        }
    }

    #[test]
    fn incremental_uid_set_is_sorted_and_deduplicated() {
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![
            fixture_message(42, "INBOX"),
            fixture_message(7, "INBOX"),
            fixture_message(42, "INBOX"),
        ];
        assert_eq!(cached_uid_set(&mailbox).as_deref(), Some("7,42"));
        mailbox.messages.clear();
        assert_eq!(cached_uid_set(&mailbox), None);
    }

    #[test]
    fn uid_search_windows_are_contiguous_bounded_and_overflow_safe() {
        fn ranges(floor: u32, ceiling: u32, order: UidSearchOrder) -> Vec<(u32, u32)> {
            let mut cursor = match order {
                UidSearchOrder::Ascending => floor,
                UidSearchOrder::Descending => ceiling,
            };
            let mut result = Vec::new();
            while let Some((range_floor, range_ceiling, has_more)) =
                next_uid_search_range(floor, ceiling, cursor, order)
            {
                assert!(range_ceiling >= range_floor);
                assert!(range_ceiling - range_floor < MAX_UID_SEARCH_RANGE_WIDTH);
                result.push((range_floor, range_ceiling));
                if !has_more {
                    break;
                }
                cursor = match order {
                    UidSearchOrder::Ascending => range_ceiling.saturating_add(1),
                    UidSearchOrder::Descending => range_floor.saturating_sub(1),
                };
            }
            result
        }

        let ascending = ranges(1, MAX_UID_SEARCH_WINDOW, UidSearchOrder::Ascending);
        assert_eq!(ascending.first(), Some(&(1, MAX_UID_SEARCH_RANGE_WIDTH)));
        assert_eq!(ascending.last(), Some(&(98_305, 100_000)));
        for pair in ascending.windows(2) {
            assert_eq!(pair[0].1 + 1, pair[1].0);
        }

        let descending = ranges(1, MAX_UID_SEARCH_WINDOW, UidSearchOrder::Descending);
        assert_eq!(descending.first(), Some(&(91_809, 100_000)));
        assert_eq!(descending.last(), Some(&(1, 1_696)));
        for pair in descending.windows(2) {
            assert_eq!(pair[1].1 + 1, pair[0].0);
        }

        assert_eq!(
            ranges(
                u32::MAX - MAX_UID_SEARCH_RANGE_WIDTH + 1,
                u32::MAX,
                UidSearchOrder::Ascending,
            ),
            vec![(u32::MAX - MAX_UID_SEARCH_RANGE_WIDTH + 1, u32::MAX)]
        );
        assert!(next_uid_search_range(0, 10, 0, UidSearchOrder::Ascending).is_none());
        assert!(next_uid_search_range(10, 9, 10, UidSearchOrder::Descending).is_none());
    }

    #[test]
    fn latest_uid_selection_fast_forwards_large_backlogs_without_skipping_small_deltas() {
        let normal = select_latest_uids(
            &BoundedUidSearch {
                uids: vec![105, 104, 100, 99],
                truncated: true,
            },
            1,
            Some(100),
        );
        assert_eq!(
            normal,
            LatestUidSelection {
                selected_uids: vec![104, 105],
                new_uid_count: Some(2),
                rebase_to_latest: false,
            }
        );

        let backlog = select_latest_uids(
            &BoundedUidSearch {
                uids: (201..=301).rev().collect(),
                truncated: true,
            },
            1,
            Some(100),
        );
        assert!(backlog.rebase_to_latest);
        assert_eq!(backlog.new_uid_count, None);
        assert_eq!(backlog.selected_uids.len(), MAX_HEADER_MESSAGES);
        assert_eq!(backlog.selected_uids.first(), Some(&301));
        assert_eq!(backlog.selected_uids.last(), Some(&202));

        let outside_window = select_latest_uids(
            &BoundedUidSearch {
                uids: vec![200],
                truncated: false,
            },
            150,
            Some(100),
        );
        assert!(outside_window.rebase_to_latest);

        let cold = select_latest_uids(
            &BoundedUidSearch {
                uids: vec![3, 2, 1],
                truncated: false,
            },
            1,
            None,
        );
        assert!(cold.rebase_to_latest);
        assert_eq!(cold.selected_uids, vec![3, 2, 1]);
    }

    #[test]
    fn status_modseq_parser_handles_quoted_mailboxes_and_unrelated_lines() {
        let response = b"* STATUS Other (HIGHESTMODSEQ 9)\r\n* STATUS \"Sent Items\" (HIGHESTMODSEQ 12345)\r\n";
        assert_eq!(
            parse_status_highest_mod_seq(response, "Sent Items"),
            Some(12_345)
        );
        assert_eq!(
            parse_status_highest_mod_seq(b"* STATUS INBOX (MESSAGES 2)\r\n", "INBOX"),
            None
        );
    }

    #[test]
    fn legacy_mailbox_snapshots_default_without_a_modseq_cursor() {
        let mailbox: CachedMailbox = serde_json::from_str(
            r#"{"schemaVersion":1,"accountId":"fixture","folder":"INBOX","uidValidity":1,"syncedAt":"unix:1","messages":[],"oldestUid":null,"hasMore":false}"#,
        )
        .expect("legacy mailbox snapshot");
        assert_eq!(mailbox.highest_mod_seq, None);
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
        assert!(!is_safe_discovered_folder("Drafts\0shadow"));
        assert!(!is_safe_discovered_folder(""));
    }

    #[test]
    fn modified_utf7_folder_names_are_decoded_without_changing_wire_names() {
        assert_eq!(
            folder_display_name("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
            "~peter/mail/台北/日本語"
        );
        assert_eq!(folder_display_name("R&-D &- QA"), "R&D & QA");
        assert_eq!(folder_display_name("INBOX"), "INBOX");
        assert_eq!(folder_display_name("Broken &invalid"), "Broken &invalid");
        assert_eq!(folder_display_name("Odd &QQ-"), "Odd &QQ-");
    }

    #[test]
    fn stale_mailbox_cache_is_reparsed_from_remote_on_next_successful_sync() {
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.schema_version = 2;
        mailbox.highest_mod_seq = Some(42);
        mailbox.oldest_uid = Some(7);
        mailbox.has_more = true;
        mailbox.messages.push(fixture_message(7, "INBOX"));

        assert!(upgrade_mailbox_cache(&mut mailbox));
        assert_eq!(mailbox.schema_version, CACHE_SCHEMA_VERSION);
        assert!(mailbox.messages.is_empty());
        assert_eq!(mailbox.highest_mod_seq, None);
        assert_eq!(mailbox.oldest_uid, None);
        assert!(!mailbox.has_more);
        assert!(!upgrade_mailbox_cache(&mut mailbox));
    }

    #[test]
    fn mailbox_names_reject_command_delimiters() {
        assert!(validate_mailbox_name("INBOX").is_ok());
        assert!(validate_mailbox_name("Sent Items").is_ok());
        assert!(validate_mailbox_name("INBOX\r\nUID FETCH 1 ALL").is_err());
        assert!(validate_mailbox_name("Drafts\0shadow").is_err());
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
            highest_mod_seq: None,
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
    fn imap_mutation_conflict_keeps_the_encrypted_flag_queue_for_retry() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-imap-conflict-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.uid_validity = Some(7);
        mailbox.messages = vec![fixture_message(7, "INBOX")];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        queue_flag_mutation(&root, "fixture-account", "INBOX", 7, "\\Seen", true).unwrap();

        let (mut session, server) = fixture_imap_session(|reader, stream| {
            respond_to_select(reader, stream, 7);
            let mut command = String::new();
            reader
                .read_line(&mut command)
                .expect("read conflicting UID STORE");
            assert!(command
                .to_ascii_uppercase()
                .contains(" UID STORE 7 +FLAGS.SILENT (\\SEEN)"));
            let tag = command
                .split_ascii_whitespace()
                .next()
                .expect("UID STORE tag");
            write!(stream, "{tag} NO [MODIFIED 7] server-side conflict\r\n")
                .expect("write conflicting UID STORE response");
            stream
                .flush()
                .expect("flush conflicting UID STORE response");
        });
        flush_queued_flags(&mut session, &root, "fixture-account");
        drop(session);
        server.join().expect("join conflicting IMAP fixture");

        let queued = load_mutations(
            &root
                .join(safe_component("fixture-account"))
                .join(MUTATION_FILE),
        )
        .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].uid, 7);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn changed_uidvalidity_discards_stale_flag_without_sending_store() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-imap-uidvalidity-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.uid_validity = Some(7);
        mailbox.messages = vec![fixture_message(7, "INBOX")];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        queue_flag_mutation(&root, "fixture-account", "INBOX", 7, "\\Seen", true).unwrap();

        let (mut session, server) = fixture_imap_session(|reader, stream| {
            respond_to_select(reader, stream, 8);
            let mut unexpected = String::new();
            let read = reader
                .read_line(&mut unexpected)
                .expect("observe UIDVALIDITY fixture completion");
            assert_eq!(read, 0, "stale mutation sent a command: {unexpected}");
        });
        flush_queued_flags(&mut session, &root, "fixture-account");
        drop(session);
        server.join().expect("join UIDVALIDITY IMAP fixture");

        let queued = load_mutations(
            &root
                .join(safe_component("fixture-account"))
                .join(MUTATION_FILE),
        )
        .unwrap();
        assert!(queued.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_mailbox_cache_recovers_from_previous_atomic_snapshot_during_migration() {
        let root =
            std::env::temp_dir().join(format!("mailgo-cache-backup-test-{}", std::process::id()));
        let first = CachedMailbox {
            schema_version: 1,
            account_id: "fixture-account".into(),
            folder: "INBOX".into(),
            uid_validity: Some(1),
            highest_mod_seq: None,
            synced_at: now_stamp(),
            messages: vec![fixture_message(1, "INBOX")],
            oldest_uid: Some(1),
            has_more: false,
        };
        let directory = root.join(safe_component("fixture-account"));
        fs::create_dir_all(&directory).unwrap();
        let primary = directory.join(CACHE_FILE);
        let backup = directory.join(format!("{CACHE_FILE}.bak"));
        fs::write(
            &backup,
            protect_cache(&serde_json::to_vec_pretty(&first).unwrap()).unwrap(),
        )
        .unwrap();
        fs::write(&primary, b"corrupt cache").unwrap();

        let recovered = load_mailbox_for_folder(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap();
        assert_eq!(recovered.messages[0].uid, 1);
        assert_eq!(
            crate::cache_db::load_message(&root, "fixture-account", "INBOX", 1)
                .unwrap()
                .unwrap()
                .uid,
            1
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_merge_migrates_legacy_snapshot_before_row_upsert() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-search-legacy-migration-test-{}",
            std::process::id()
        ));
        let legacy = CachedMailbox {
            schema_version: 1,
            account_id: "fixture-account".into(),
            folder: "INBOX".into(),
            uid_validity: Some(5),
            highest_mod_seq: None,
            synced_at: now_stamp(),
            messages: vec![fixture_message(1, "INBOX")],
            oldest_uid: Some(1),
            has_more: false,
        };
        let directory = root.join(safe_component("fixture-account"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(CACHE_FILE),
            protect_cache(&serde_json::to_vec(&legacy).unwrap()).unwrap(),
        )
        .unwrap();

        save_search_messages(
            &root,
            "fixture-account",
            Some(5),
            &[fixture_message(2, "INBOX")],
        )
        .unwrap();

        assert_eq!(
            crate::cache_db::load_message(&root, "fixture-account", "INBOX", 1)
                .unwrap()
                .unwrap()
                .uid,
            1
        );
        assert_eq!(
            crate::cache_db::load_message(&root, "fixture-account", "INBOX", 2)
                .unwrap()
                .unwrap()
                .uid,
            2
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn move_migrates_legacy_snapshot_before_exact_transaction() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-move-legacy-migration-test-{}",
            std::process::id()
        ));
        let legacy = CachedMailbox {
            schema_version: 1,
            account_id: "fixture-account".into(),
            folder: "INBOX".into(),
            uid_validity: Some(5),
            highest_mod_seq: None,
            synced_at: now_stamp(),
            messages: vec![fixture_message(8, "INBOX")],
            oldest_uid: Some(8),
            has_more: false,
        };
        let directory = root.join(safe_component("fixture-account"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(CACHE_FILE),
            protect_cache(&serde_json::to_vec(&legacy).unwrap()).unwrap(),
        )
        .unwrap();

        update_cached_move(&root, "fixture-account", "INBOX", 8, "Archive").unwrap();

        assert!(
            crate::cache_db::load_message(&root, "fixture-account", "INBOX", 8)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::cache_db::load_message(&root, "fixture-account", "Archive", 8)
                .unwrap()
                .unwrap()
                .folder,
            "Archive"
        );
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
            highest_mod_seq: None,
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
