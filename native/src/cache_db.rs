use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use rand::RngCore;
use rusqlite::ffi::ErrorCode;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior, MAIN_DB};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mail::{CachedMailbox, CachedMessage, CACHE_SCHEMA_VERSION};

const DATABASE_FILE: &str = "mail-index-v1.sqlite3";
const DATABASE_BACKUP_FILE: &str = "mail-index-v1.sqlite3.backup";
const DATABASE_BACKUP_PREVIOUS_FILE: &str = "mail-index-v1.sqlite3.backup.previous";
const DATABASE_BACKUP_PENDING_FILE: &str = "mail-index-v1.sqlite3.backup.pending";
const SEARCH_KEY_FILE: &str = "search-index-key-v1.bin";
const SEARCH_KEY_BYTES: usize = 32;
const SEARCH_KEY_MAX_FILE_BYTES: usize = 4096;
const SEARCH_INDEX_VERSION: i64 = 1;
const SEARCH_INDEX_BATCH_SIZE: usize = 96;
const SEARCH_INDEX_FOREGROUND_BATCH_SIZE: usize = 24;
const MAX_SEARCH_INDEX_CHARACTERS: usize = 16 * 1024;
const MAX_SEARCH_WORD_CHARACTERS: usize = 256;
const MAX_SEARCH_TERMS_PER_MESSAGE: usize = 512;
const MAX_SEARCH_QUERY_TERMS: usize = 32;
const MAX_LOCAL_SEARCH_CANDIDATES: usize = 2000;
const MAX_RECIPIENT_INDEX_CANDIDATES: usize = 384;
const MAX_RECIPIENT_RECENT_CANDIDATES: usize = 128;
const MAX_RECIPIENT_FALLBACK_CANDIDATES: usize = 512;
const MAX_RECIPIENT_SUGGESTIONS: usize = 20;
const DATABASE_SCHEMA_VERSION: i64 = 4;
const MAX_PAGE_SIZE: usize = 500;
const MAX_SYNC_SUMMARIES: usize = 10_000;
const MAX_ENCRYPTED_ROW_BYTES: usize = 8 * 1024 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const LIST_INDEX_BATCH_SIZE: usize = 48;
const ENCRYPTION_MIGRATION_BATCH_SIZE: usize = 32;
const ENCRYPTION_MIGRATION_BATCH_BYTES: usize = 16 * 1024 * 1024;
const CURRENT_DATABASE_ENCRYPTION_VERSION: i64 = 1;

static INITIALIZED_DATABASES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static DATABASE_ACCESS: OnceLock<RwLock<()>> = OnceLock::new();
static BACKUP_ACCESS: OnceLock<Mutex<()>> = OnceLock::new();
static SEARCH_KEYS: OnceLock<Mutex<HashMap<PathBuf, [u8; SEARCH_KEY_BYTES]>>> = OnceLock::new();
static SEARCH_INDEX_RUNNING: AtomicBool = AtomicBool::new(false);
static SEARCH_INDEX_REQUESTED: AtomicBool = AtomicBool::new(false);
static LIST_INDEX_RUNNING: AtomicBool = AtomicBool::new(false);
static LIST_INDEX_REQUESTED: AtomicBool = AtomicBool::new(false);
static ENCRYPTION_MIGRATION_RUNNING: AtomicBool = AtomicBool::new(false);

type HmacSha256 = Hmac<Sha256>;

struct SearchIndexerRun;

impl Drop for SearchIndexerRun {
    fn drop(&mut self) {
        SEARCH_INDEX_RUNNING.store(false, Ordering::Release);
    }
}

struct ListIndexerRun;

impl Drop for ListIndexerRun {
    fn drop(&mut self) {
        LIST_INDEX_RUNNING.store(false, Ordering::Release);
    }
}

struct EncryptionMigrationRun;

impl Drop for EncryptionMigrationRun {
    fn drop(&mut self) {
        ENCRYPTION_MIGRATION_RUNNING.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxMetadata {
    schema_version: u32,
    account_id: String,
    folder: String,
    uid_validity: Option<u32>,
    highest_mod_seq: Option<u64>,
    synced_at: String,
    oldest_uid: Option<u32>,
    has_more: bool,
}

impl From<&CachedMailbox> for MailboxMetadata {
    fn from(mailbox: &CachedMailbox) -> Self {
        Self {
            schema_version: mailbox.schema_version,
            account_id: mailbox.account_id.clone(),
            folder: mailbox.folder.clone(),
            uid_validity: mailbox.uid_validity,
            highest_mod_seq: mailbox.highest_mod_seq,
            synced_at: mailbox.synced_at.clone(),
            oldest_uid: mailbox.oldest_uid,
            has_more: mailbox.has_more,
        }
    }
}

impl MailboxMetadata {
    fn into_mailbox(self, messages: Vec<CachedMessage>) -> CachedMailbox {
        CachedMailbox {
            schema_version: self.schema_version,
            account_id: self.account_id,
            folder: self.folder,
            uid_validity: self.uid_validity,
            highest_mod_seq: self.highest_mod_seq,
            synced_at: self.synced_at,
            messages,
            oldest_uid: self.oldest_uid,
            has_more: self.has_more,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MailboxPage {
    pub mailbox: CachedMailbox,
    pub local_has_more: bool,
    pub remote_has_more: bool,
    pub total_cached: usize,
    pub revision: u64,
}

#[derive(Debug)]
pub struct LocalSearchResult {
    pub messages: Vec<CachedMessage>,
    pub truncated: bool,
    pub indexing: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipientSuggestion {
    pub name: String,
    pub email: String,
    pub frequency: u32,
    pub last_seen: Option<String>,
}

#[derive(Debug)]
pub struct RecipientSuggestionResult {
    pub suggestions: Vec<RecipientSuggestion>,
    pub truncated: bool,
    pub indexing: bool,
}

#[derive(Debug)]
pub struct SearchIndexProgress {
    pub indexed: usize,
    pub has_more: bool,
}

pub fn database_path(cache_root: &Path) -> PathBuf {
    cache_root.join(DATABASE_FILE)
}

fn backup_path(cache_root: &Path) -> PathBuf {
    cache_root.join(DATABASE_BACKUP_FILE)
}

fn search_key_path(cache_root: &Path) -> PathBuf {
    cache_root.join(SEARCH_KEY_FILE)
}

fn identity_key(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn folder_identity_key(account_id: &str, folder: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(account_id.as_bytes());
    hasher.update([0]);
    for character in folder.chars().flat_map(char::to_lowercase) {
        let mut encoded = [0u8; 4];
        hasher.update(character.encode_utf8(&mut encoded).as_bytes());
    }
    hasher.finalize().into()
}

fn now_epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn write_protected_search_key(path: &Path, key: &[u8; SEARCH_KEY_BYTES]) -> Result<()> {
    let encrypted = crate::sync::protect_cache(key).context("protect local search index key")?;
    if encrypted.len() > SEARCH_KEY_MAX_FILE_BYTES {
        return Err(anyhow!("protected local search index key is too large"));
    }
    let pending = path.with_extension("bin.pending");
    remove_file_if_exists(&pending)?;
    fs::write(&pending, encrypted)
        .with_context(|| format!("write local search index key {}", pending.display()))?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pending)
        .with_context(|| format!("open local search index key {}", pending.display()))?
        .sync_all()
        .with_context(|| format!("flush local search index key {}", pending.display()))?;
    fs::rename(&pending, path)
        .with_context(|| format!("commit local search index key {}", path.display()))?;
    Ok(())
}

fn generate_search_key(path: &Path) -> Result<[u8; SEARCH_KEY_BYTES]> {
    let mut generated = [0u8; SEARCH_KEY_BYTES];
    rand::thread_rng().fill_bytes(&mut generated);
    write_protected_search_key(path, &generated)?;
    Ok(generated)
}

fn quarantine_invalid_search_key(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("local search index key has no parent directory"))?;
    let stamp = format!("{}-{}", now_epoch_millis(), std::process::id());
    let mut target = parent.join(format!("{SEARCH_KEY_FILE}.invalid-{stamp}"));
    for sequence in 1..=100u8 {
        if !target.exists() {
            break;
        }
        target = parent.join(format!("{SEARCH_KEY_FILE}.invalid-{stamp}-{sequence}"));
    }
    if target.exists() {
        return Err(anyhow!("too many invalid local search index key files"));
    }
    fs::rename(path, &target).context("quarantine invalid local search index key")?;
    Ok(target)
}

fn load_search_key(cache_root: &Path) -> Result<[u8; SEARCH_KEY_BYTES]> {
    fs::create_dir_all(cache_root)
        .with_context(|| format!("create mail cache directory {}", cache_root.display()))?;
    let path = search_key_path(cache_root);
    let keys = SEARCH_KEYS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut keys = keys
        .lock()
        .map_err(|_| anyhow!("local search index key lock poisoned"))?;
    if let Some(key) = keys.get(&path) {
        return Ok(*key);
    }
    let key = match fs::read(&path) {
        Ok(encrypted) => match (|| -> Result<[u8; SEARCH_KEY_BYTES]> {
            if encrypted.len() > SEARCH_KEY_MAX_FILE_BYTES {
                return Err(anyhow!("protected local search index key is too large"));
            }
            let decoded = crate::sync::unprotect_cache(&encrypted)
                .context("unprotect local search index key")?;
            decoded
                .try_into()
                .map_err(|_| anyhow!("local search index key has an invalid length"))
        })() {
            Ok(decoded) => decoded,
            Err(error) => {
                quarantine_invalid_search_key(&path)?;
                tracing::warn!(error = %error, "replaced an unreadable local search index key");
                generate_search_key(&path)?
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => generate_search_key(&path)?,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read local search index key {}", path.display()))
        }
    };
    keys.insert(path, key);
    Ok(key)
}

fn normalize_search_words<'a>(fields: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut characters = 0usize;
    for field in fields {
        for character in field.chars() {
            if characters >= MAX_SEARCH_INDEX_CHARACTERS {
                break;
            }
            characters += 1;
            if character.is_alphanumeric() || matches!(character, '@' | '.' | '_' | '+' | '-') {
                if current.chars().count() < MAX_SEARCH_WORD_CHARACTERS {
                    current.extend(character.to_lowercase());
                }
            } else if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        if characters >= MAX_SEARCH_INDEX_CHARACTERS {
            break;
        }
    }
    words
}

fn message_search_words(message: &CachedMessage) -> Vec<String> {
    normalize_search_words(
        [
            message.subject.as_str(),
            message.sender_name.as_str(),
            message.sender_email.as_str(),
        ]
        .into_iter()
        .chain(message.to.iter().map(String::as_str))
        .chain(message.cc.iter().map(String::as_str))
        .chain([message.preview.as_str(), message.text_body.as_str()]),
    )
}

fn query_search_words(query: &str) -> Vec<String> {
    normalize_search_words([query])
}

fn search_grams(words: &[String], query: bool, limit: usize) -> Vec<String> {
    let mut grams = Vec::new();
    let mut seen = HashSet::new();
    for word in words {
        let characters = word
            .chars()
            .take(MAX_SEARCH_WORD_CHARACTERS)
            .collect::<Vec<_>>();
        let widths: &[usize] = if query {
            if characters.len() >= 3 {
                &[3]
            } else if characters.len() == 2 {
                &[2]
            } else {
                &[]
            }
        } else {
            &[2, 3]
        };
        for width in widths {
            if characters.len() < *width {
                continue;
            }
            for window in characters.windows(*width) {
                let gram = format!("{width}:{}", window.iter().collect::<String>());
                if seen.insert(gram.clone()) {
                    grams.push(gram);
                    if grams.len() >= limit {
                        return grams;
                    }
                }
            }
        }
    }
    grams
}

fn blind_index_term(
    key: &[u8; SEARCH_KEY_BYTES],
    namespace: &[u8],
    gram: &str,
) -> Result<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(key).context("initialize local search HMAC")?;
    mac.update(namespace);
    mac.update(gram.as_bytes());
    Ok(mac.finalize().into_bytes().into())
}

fn blind_search_term(key: &[u8; SEARCH_KEY_BYTES], gram: &str) -> Result<[u8; 32]> {
    blind_index_term(key, b"mailgo-search-v1\0", gram)
}

fn blind_recipient_term(key: &[u8; SEARCH_KEY_BYTES], gram: &str) -> Result<[u8; 32]> {
    blind_index_term(key, b"mailgo-recipient-v1\0", gram)
}

fn message_search_terms(
    key: &[u8; SEARCH_KEY_BYTES],
    message: &CachedMessage,
) -> Result<Vec<[u8; 32]>> {
    search_grams(
        &message_search_words(message),
        false,
        MAX_SEARCH_TERMS_PER_MESSAGE,
    )
    .iter()
    .map(|gram| blind_search_term(key, gram))
    .collect()
}

fn recipient_search_words(message: &CachedMessage) -> Vec<String> {
    normalize_search_words(
        [message.sender_name.as_str(), message.sender_email.as_str()]
            .into_iter()
            .chain(message.to.iter().map(String::as_str))
            .chain(message.cc.iter().map(String::as_str)),
    )
}

fn recipient_search_terms(
    key: &[u8; SEARCH_KEY_BYTES],
    message: &CachedMessage,
) -> Result<Vec<[u8; 32]>> {
    search_grams(
        &recipient_search_words(message),
        false,
        MAX_SEARCH_TERMS_PER_MESSAGE,
    )
    .iter()
    .map(|gram| blind_recipient_term(key, gram))
    .collect()
}

fn query_search_terms(key: &[u8; SEARCH_KEY_BYTES], words: &[String]) -> Result<Vec<[u8; 32]>> {
    search_grams(words, true, MAX_SEARCH_QUERY_TERMS)
        .iter()
        .map(|gram| blind_search_term(key, gram))
        .collect()
}

fn recipient_query_terms(key: &[u8; SEARCH_KEY_BYTES], words: &[String]) -> Result<Vec<[u8; 32]>> {
    search_grams(words, true, MAX_SEARCH_QUERY_TERMS)
        .iter()
        .map(|gram| blind_recipient_term(key, gram))
        .collect()
}

fn open(cache_root: &Path) -> Result<Connection> {
    fs::create_dir_all(cache_root)
        .with_context(|| format!("create mail cache directory {}", cache_root.display()))?;
    let path = database_path(cache_root);
    let database_existed = path.exists();
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open indexed mail cache {}", path.display()))?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -8192i64)?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "wal_autocheckpoint", 500i64)?;

    let initialized = INITIALIZED_DATABASES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut initialized = initialized
        .lock()
        .map_err(|_| anyhow!("indexed mail cache initialization lock poisoned"))?;
    if !database_existed {
        initialized.remove(&path);
    }
    if !initialized.contains(&path) {
        initialize(&mut connection)?;
        initialized.insert(path);
    }
    Ok(connection)
}

fn is_sqlite_corruption(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| match error {
                rusqlite::Error::SqliteFailure(failure, _) => matches!(
                    failure.code,
                    ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
                ),
                _ => false,
            })
    })
}

fn backup_maintenance_error(error: anyhow::Error) -> anyhow::Error {
    // Backup destinations are separate SQLite files. Keep their error chain opaque so a damaged
    // pending/previous copy can never be mistaken for corruption in the live primary database.
    anyhow!("indexed mail cache backup maintenance failed: {error:#}")
}

fn with_recovery<T>(
    cache_root: &Path,
    mut operation: impl FnMut(&mut Connection) -> Result<T>,
) -> Result<T> {
    if !database_path(cache_root).exists()
        && [
            backup_path(cache_root),
            cache_root.join(DATABASE_BACKUP_PREVIOUS_FILE),
            cache_root.join(DATABASE_BACKUP_PENDING_FILE),
        ]
        .iter()
        .any(|path| path.exists())
    {
        recover_database(cache_root)?;
    }
    let access = DATABASE_ACCESS.get_or_init(|| RwLock::new(()));
    let first_attempt = {
        let _read_guard = access
            .read()
            .map_err(|_| anyhow!("indexed mail cache access lock poisoned"))?;
        let mut connection = match open(cache_root) {
            Ok(connection) => connection,
            Err(error) => {
                if !is_sqlite_corruption(&error) {
                    return Err(error);
                }
                drop(_read_guard);
                recover_database(cache_root)?;
                let _retry_guard = access
                    .read()
                    .map_err(|_| anyhow!("indexed mail cache access lock poisoned"))?;
                let mut connection = open(cache_root)?;
                return operation(&mut connection)
                    .context("retry indexed mail cache operation after recovery");
            }
        };
        operation(&mut connection)
    };
    match first_attempt {
        Ok(value) => Ok(value),
        Err(error) if is_sqlite_corruption(&error) => {
            recover_database(cache_root)?;
            let _read_guard = access
                .read()
                .map_err(|_| anyhow!("indexed mail cache access lock poisoned"))?;
            let mut connection = open(cache_root)?;
            operation(&mut connection).context("retry indexed mail cache operation after recovery")
        }
        Err(error) => Err(error),
    }
}

fn validate_database_file(path: &Path) -> Result<bool> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "open indexed mail cache recovery candidate {}",
            path.display()
        )
    })?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let check: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if check != "ok" {
        return Ok(false);
    }
    let schema_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if schema_version <= 0 || schema_version > DATABASE_SCHEMA_VERSION {
        return Ok(false);
    }
    let required_tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'table' AND name IN ('mailbox_meta', 'messages', 'message_list')",
        [],
        |row| row.get(0),
    )?;
    Ok(required_tables == if schema_version >= 3 { 3 } else { 2 })
}

fn remove_initialized_path(path: &Path) -> Result<()> {
    let initialized = INITIALIZED_DATABASES.get_or_init(|| Mutex::new(HashSet::new()));
    initialized
        .lock()
        .map_err(|_| anyhow!("indexed mail cache initialization lock poisoned"))?
        .remove(path);
    Ok(())
}

fn quarantine_database_files(cache_root: &Path) -> Result<Vec<PathBuf>> {
    let stamp = format!("{}-{}", now_epoch_millis(), std::process::id());
    let paths = [
        database_path(cache_root),
        cache_root.join(format!("{DATABASE_FILE}-wal")),
        cache_root.join(format!("{DATABASE_FILE}-shm")),
    ];
    let mut quarantined = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("indexed mail cache path is not valid UTF-8"))?;
        let mut target = cache_root.join(format!("{file_name}.corrupt-{stamp}"));
        for sequence in 1..=100u8 {
            if !target.exists() {
                break;
            }
            target = cache_root.join(format!("{file_name}.corrupt-{stamp}-{sequence}"));
        }
        if target.exists() {
            return Err(anyhow!("too many indexed mail cache recovery files"));
        }
        fs::rename(&path, &target)
            .with_context(|| format!("quarantine damaged mail cache {}", path.display()))?;
        quarantined.push(target);
    }
    Ok(quarantined)
}

fn restore_candidate(candidate: &Path, primary: &Path) -> Result<()> {
    let temporary = primary.with_extension("sqlite3.recovery.pending");
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("remove stale recovery file {}", temporary.display()))
        }
    }
    fs::copy(candidate, &temporary).with_context(|| {
        format!(
            "copy indexed mail cache recovery candidate {}",
            candidate.display()
        )
    })?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("open recovery file {}", temporary.display()))?
        .sync_all()
        .with_context(|| format!("flush recovery file {}", temporary.display()))?;
    if !validate_database_file(&temporary)? {
        return Err(anyhow!(
            "indexed mail cache recovery candidate became invalid"
        ));
    }
    fs::rename(&temporary, primary).with_context(|| {
        format!(
            "commit indexed mail cache recovery to {}",
            primary.display()
        )
    })?;
    Ok(())
}

fn recover_database(cache_root: &Path) -> Result<()> {
    let access = DATABASE_ACCESS.get_or_init(|| RwLock::new(()));
    let _write_guard = access
        .write()
        .map_err(|_| anyhow!("indexed mail cache access lock poisoned"))?;
    fs::create_dir_all(cache_root)
        .with_context(|| format!("create mail cache directory {}", cache_root.display()))?;
    let primary = database_path(cache_root);

    if primary.exists() {
        match validate_database_file(&primary) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if is_sqlite_corruption(&error) => {}
            Err(error) => return Err(error).context("verify indexed mail cache before recovery"),
        }
    }

    let candidates = [
        backup_path(cache_root),
        cache_root.join(DATABASE_BACKUP_PREVIOUS_FILE),
        cache_root.join(DATABASE_BACKUP_PENDING_FILE),
    ];
    let mut selected = None;
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        match validate_database_file(&candidate) {
            Ok(true) => {
                selected = Some(candidate);
                break;
            }
            Ok(false) => {
                tracing::warn!("indexed mail cache recovery candidate failed integrity check")
            }
            Err(error) if is_sqlite_corruption(&error) => {
                tracing::warn!("indexed mail cache recovery candidate is damaged")
            }
            Err(error) => {
                return Err(error).context("validate indexed mail cache recovery candidate")
            }
        }
    }

    let quarantined = quarantine_database_files(cache_root)?;
    remove_initialized_path(&primary)?;
    if let Some(candidate) = selected {
        restore_candidate(&candidate, &primary)?;
        tracing::warn!(
            quarantined_files = quarantined.len(),
            "recovered damaged indexed mail cache from a validated backup"
        );
    } else {
        tracing::warn!(
            quarantined_files = quarantined.len(),
            "damaged indexed mail cache had no valid backup; rebuilding the local index"
        );
    }
    Ok(())
}

fn backup_is_due(cache_root: &Path) -> bool {
    let Ok(modified) = fs::metadata(backup_path(cache_root)).and_then(|value| value.modified())
    else {
        return true;
    };
    modified
        .elapsed()
        .map_or(true, |elapsed| elapsed >= Duration::from_secs(60))
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

fn rotate_backup(cache_root: &Path) -> Result<()> {
    let backup = backup_path(cache_root);
    let previous = cache_root.join(DATABASE_BACKUP_PREVIOUS_FILE);
    let pending = cache_root.join(DATABASE_BACKUP_PENDING_FILE);
    remove_file_if_exists(&previous)?;
    if backup.exists() {
        fs::rename(&backup, &previous)
            .with_context(|| format!("rotate indexed mail cache backup {}", backup.display()))?;
    }
    if let Err(error) = fs::rename(&pending, &backup) {
        if previous.exists() && !backup.exists() {
            let _ = fs::rename(&previous, &backup);
        }
        return Err(error)
            .with_context(|| format!("commit indexed mail cache backup {}", backup.display()));
    }
    Ok(())
}

fn refresh_backup_with_connection(
    connection: &Connection,
    cache_root: &Path,
    force: bool,
) -> Result<()> {
    if !force && !backup_is_due(cache_root) {
        return Ok(());
    }
    let backup_access = BACKUP_ACCESS.get_or_init(|| Mutex::new(()));
    let _backup_guard = backup_access
        .lock()
        .map_err(|_| anyhow!("indexed mail cache backup lock poisoned"))?;
    if !force && !backup_is_due(cache_root) {
        return Ok(());
    }
    let pending = cache_root.join(DATABASE_BACKUP_PENDING_FILE);
    remove_file_if_exists(&pending)?;
    connection
        .backup(MAIN_DB, &pending, None)
        .context("create online indexed mail cache backup")?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pending)
        .with_context(|| format!("open backup {}", pending.display()))?
        .sync_all()
        .with_context(|| format!("flush backup {}", pending.display()))?;
    if !validate_database_file(&pending)? {
        return Err(anyhow!(
            "new indexed mail cache backup failed integrity check"
        ));
    }
    rotate_backup(cache_root)
}

pub fn refresh_backup(cache_root: &Path) -> Result<()> {
    with_recovery(cache_root, |connection| {
        refresh_backup_with_connection(connection, cache_root, false)
            .map_err(backup_maintenance_error)
    })
}

fn initialize(connection: &mut Connection) -> Result<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS mailbox_meta (
            account_key BLOB NOT NULL,
            folder_key BLOB NOT NULL,
            payload BLOB NOT NULL,
            encryption_version INTEGER NOT NULL DEFAULT 0,
            revision INTEGER NOT NULL DEFAULT 1,
            message_count INTEGER NOT NULL DEFAULT 0,
            list_count INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (account_key, folder_key)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS messages (
            account_key BLOB NOT NULL,
            folder_key BLOB NOT NULL,
            uid INTEGER NOT NULL CHECK (uid > 0 AND uid <= 4294967295),
            payload_hash BLOB NOT NULL,
            payload BLOB NOT NULL,
            encryption_version INTEGER NOT NULL DEFAULT 0,
            search_version INTEGER NOT NULL DEFAULT 0,
            recipient_version INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (account_key, folder_key, uid),
            FOREIGN KEY (account_key, folder_key)
                REFERENCES mailbox_meta(account_key, folder_key) ON DELETE CASCADE
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS messages_page
            ON messages(account_key, folder_key, uid DESC);",
    )?;
    let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > DATABASE_SCHEMA_VERSION {
        return Err(anyhow!(
            "indexed mail cache was created by a newer MailGo version"
        ));
    }
    let message_columns = {
        let mut statement = connection.prepare("PRAGMA table_info(messages)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = HashSet::new();
        for column in columns {
            found.insert(column?.to_ascii_lowercase());
        }
        found
    };
    if !message_columns.contains("search_version") {
        connection.execute(
            "ALTER TABLE messages ADD COLUMN search_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !message_columns.contains("recipient_version") {
        connection.execute(
            "ALTER TABLE messages ADD COLUMN recipient_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !message_columns.contains("encryption_version") {
        connection.execute(
            "ALTER TABLE messages ADD COLUMN encryption_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    let had_message_list: bool = connection.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM sqlite_master
           WHERE type = 'table' AND name = 'message_list'
         )",
        [],
        |row| row.get(0),
    )?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_list (
            account_key BLOB NOT NULL,
            folder_key BLOB NOT NULL,
            uid INTEGER NOT NULL CHECK (uid > 0 AND uid <= 4294967295),
            payload BLOB NOT NULL,
            encryption_version INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account_key, folder_key, uid),
            FOREIGN KEY (account_key, folder_key, uid)
                REFERENCES messages(account_key, folder_key, uid) ON DELETE CASCADE
        ) WITHOUT ROWID;",
    )?;
    let message_list_columns = {
        let mut statement = connection.prepare("PRAGMA table_info(message_list)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = HashSet::new();
        for column in columns {
            found.insert(column?.to_ascii_lowercase());
        }
        found
    };
    if !message_list_columns.contains("encryption_version") {
        connection.execute(
            "ALTER TABLE message_list ADD COLUMN encryption_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    let metadata_columns = {
        let mut statement = connection.prepare("PRAGMA table_info(mailbox_meta)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = HashSet::new();
        for column in columns {
            found.insert(column?.to_ascii_lowercase());
        }
        found
    };
    if !metadata_columns.contains("message_count") {
        connection.execute(
            "ALTER TABLE mailbox_meta ADD COLUMN message_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !metadata_columns.contains("list_count") {
        connection.execute(
            "ALTER TABLE mailbox_meta ADD COLUMN list_count INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !metadata_columns.contains("encryption_version") {
        connection.execute(
            "ALTER TABLE mailbox_meta ADD COLUMN encryption_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if current < 3 {
        connection.execute(
            "UPDATE mailbox_meta
             SET message_count = (
               SELECT COUNT(*) FROM messages
               WHERE messages.account_key = mailbox_meta.account_key
                 AND messages.folder_key = mailbox_meta.folder_key
             ),
             list_count = (
               SELECT COUNT(*) FROM message_list
               WHERE message_list.account_key = mailbox_meta.account_key
                 AND message_list.folder_key = mailbox_meta.folder_key
             )",
            [],
        )?;
    } else if !had_message_list {
        connection.execute("UPDATE mailbox_meta SET list_count = 0", [])?;
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_search_terms (
            term BLOB NOT NULL,
            account_key BLOB NOT NULL,
            folder_key BLOB NOT NULL,
            uid INTEGER NOT NULL,
            PRIMARY KEY (term, account_key, folder_key, uid),
            FOREIGN KEY (account_key, folder_key, uid)
                REFERENCES messages(account_key, folder_key, uid) ON DELETE CASCADE
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS message_search_terms_owner
            ON message_search_terms(account_key, folder_key, uid);
        CREATE TABLE IF NOT EXISTS recipient_search_terms (
            term BLOB NOT NULL,
            account_key BLOB NOT NULL,
            folder_key BLOB NOT NULL,
            uid INTEGER NOT NULL,
            PRIMARY KEY (term, account_key, folder_key, uid),
            FOREIGN KEY (account_key, folder_key, uid)
                REFERENCES messages(account_key, folder_key, uid) ON DELETE CASCADE
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS recipient_search_terms_owner
            ON recipient_search_terms(account_key, folder_key, uid);
        CREATE TABLE IF NOT EXISTS search_index_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            key_fingerprint BLOB NOT NULL,
            index_version INTEGER NOT NULL
        );",
    )?;
    connection.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS mailbox_meta_encryption_insert
           AFTER INSERT ON mailbox_meta
           WHEN NEW.encryption_version !=
             CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                  THEN 1 ELSE 0 END
           BEGIN
             UPDATE mailbox_meta SET encryption_version =
               CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                    THEN 1 ELSE 0 END
             WHERE account_key = NEW.account_key AND folder_key = NEW.folder_key;
           END;
         CREATE TRIGGER IF NOT EXISTS mailbox_meta_encryption_update
           AFTER UPDATE OF payload ON mailbox_meta
           WHEN NEW.encryption_version !=
             CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                  THEN 1 ELSE 0 END
           BEGIN
             UPDATE mailbox_meta SET encryption_version =
               CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                    THEN 1 ELSE 0 END
             WHERE account_key = NEW.account_key AND folder_key = NEW.folder_key;
           END;
         CREATE TRIGGER IF NOT EXISTS messages_encryption_insert
           AFTER INSERT ON messages
           WHEN NEW.encryption_version !=
             CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                  THEN 1 ELSE 0 END
           BEGIN
             UPDATE messages SET encryption_version =
               CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                    THEN 1 ELSE 0 END
             WHERE account_key = NEW.account_key AND folder_key = NEW.folder_key
               AND uid = NEW.uid;
           END;
         CREATE TRIGGER IF NOT EXISTS messages_encryption_update
           AFTER UPDATE OF payload ON messages
           WHEN NEW.encryption_version !=
             CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                  THEN 1 ELSE 0 END
           BEGIN
             UPDATE messages SET encryption_version =
               CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                    THEN 1 ELSE 0 END
             WHERE account_key = NEW.account_key AND folder_key = NEW.folder_key
               AND uid = NEW.uid;
           END;
         CREATE TRIGGER IF NOT EXISTS message_list_encryption_insert
           AFTER INSERT ON message_list
           WHEN NEW.encryption_version !=
             CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                  THEN 1 ELSE 0 END
           BEGIN
             UPDATE message_list SET encryption_version =
               CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                    THEN 1 ELSE 0 END
             WHERE account_key = NEW.account_key AND folder_key = NEW.folder_key
               AND uid = NEW.uid;
           END;
         CREATE TRIGGER IF NOT EXISTS message_list_encryption_update
           AFTER UPDATE OF payload ON message_list
           WHEN NEW.encryption_version !=
             CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                  THEN 1 ELSE 0 END
           BEGIN
             UPDATE message_list SET encryption_version =
               CASE WHEN substr(NEW.payload, 1, 15) = X'4D41494C474F2D43414348452D3100'
                    THEN 1 ELSE 0 END
             WHERE account_key = NEW.account_key AND folder_key = NEW.folder_key
               AND uid = NEW.uid;
           END;
         CREATE INDEX IF NOT EXISTS mailbox_meta_encryption_migration
           ON mailbox_meta(encryption_version, account_key, folder_key);
         CREATE INDEX IF NOT EXISTS messages_encryption_migration
           ON messages(encryption_version, account_key, folder_key, uid);
         CREATE INDEX IF NOT EXISTS message_list_encryption_migration
           ON message_list(encryption_version, account_key, folder_key, uid);",
    )?;
    connection.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
    Ok(())
}

fn encrypt_json<T: Serialize>(value: &T) -> Result<(Vec<u8>, [u8; 32])> {
    let serialized = serde_json::to_vec(value)?;
    if serialized.len() > MAX_ENCRYPTED_ROW_BYTES {
        return Err(anyhow!("indexed mail cache row is too large"));
    }
    let digest = Sha256::digest(&serialized).into();
    let encrypted = crate::sync::protect_database_cache(&serialized)?;
    if encrypted.len() > MAX_ENCRYPTED_ROW_BYTES {
        return Err(anyhow!("encrypted indexed mail cache row is too large"));
    }
    Ok((encrypted, digest))
}

fn decrypt_json<T: for<'de> Deserialize<'de>>(encrypted: &[u8], kind: &str) -> Result<T> {
    if encrypted.len() > MAX_ENCRYPTED_ROW_BYTES {
        return Err(anyhow!("encrypted indexed mail cache {kind} is too large"));
    }
    let serialized = crate::sync::unprotect_database_cache(encrypted)
        .with_context(|| format!("decrypt indexed mail cache {kind}"))?;
    serde_json::from_slice(&serialized).with_context(|| format!("parse indexed mail cache {kind}"))
}

fn read_metadata(
    connection: &Connection,
    account_key: &[u8],
    folder_key: &[u8],
) -> Result<Option<(MailboxMetadata, u64)>> {
    let row = connection
        .query_row(
            "SELECT payload, revision FROM mailbox_meta
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((payload, revision)) = row else {
        return Ok(None);
    };
    let metadata = decrypt_json(&payload, "mailbox metadata")?;
    Ok(Some((metadata, revision.max(0) as u64)))
}

fn read_metadata_for_page(
    connection: &Connection,
    account_key: &[u8],
    folder_key: &[u8],
) -> Result<Option<(MailboxMetadata, u64, usize, bool)>> {
    let row = connection
        .query_row(
            "SELECT payload, revision, message_count, list_count FROM mailbox_meta
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((payload, revision, message_count, list_count)) = row else {
        return Ok(None);
    };
    let metadata = decrypt_json(&payload, "mailbox metadata")?;
    Ok(Some((
        metadata,
        revision.max(0) as u64,
        message_count.max(0) as usize,
        list_count >= message_count && message_count >= 0,
    )))
}

fn validate_identity(metadata: &MailboxMetadata, account_id: &str, folder: &str) -> Result<()> {
    if metadata.account_id != account_id || !metadata.folder.eq_ignore_ascii_case(folder) {
        return Err(anyhow!("indexed mail cache identity mismatch"));
    }
    Ok(())
}

fn decrypt_message(
    payload: &[u8],
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<CachedMessage> {
    let mut message: CachedMessage = decrypt_json(payload, "message")?;
    if message.account_id != account_id
        || !message.folder.eq_ignore_ascii_case(folder)
        || message.uid != uid
    {
        return Err(anyhow!("indexed message cache identity mismatch"));
    }
    crate::mail::bound_cached_message(&mut message);
    Ok(message)
}

fn message_for_list(message: &CachedMessage) -> CachedMessage {
    let mut summary = message.clone();
    summary.text_body.clear();
    summary.html_body = None;
    summary.raw_path = None;
    for attachment in &mut summary.attachments {
        attachment.cache_path = None;
    }
    summary
}

fn encrypt_message_payloads(message: &CachedMessage) -> Result<(Vec<u8>, [u8; 32], Vec<u8>)> {
    let (payload, digest) = encrypt_json(message)?;
    let list_payload = if message.text_body.is_empty()
        && message.html_body.is_none()
        && message.raw_path.is_none()
        && message
            .attachments
            .iter()
            .all(|attachment| attachment.cache_path.is_none())
    {
        payload.clone()
    } else {
        encrypt_json(&message_for_list(message))?.0
    };
    Ok((payload, digest, list_payload))
}

fn decrypt_list_message(
    payload: &[u8],
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<CachedMessage> {
    let message = decrypt_message(payload, account_id, folder, uid)?;
    Ok(message_for_list(&message))
}

fn search_key_fingerprint(key: &[u8; SEARCH_KEY_BYTES]) -> [u8; 32] {
    Sha256::digest(key).into()
}

fn ensure_search_key_state(
    connection: &mut Connection,
    key: &[u8; SEARCH_KEY_BYTES],
) -> Result<()> {
    let fingerprint = search_key_fingerprint(key);
    let stored = connection
        .query_row(
            "SELECT key_fingerprint, index_version FROM search_index_meta WHERE id = 1",
            [],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if stored
        .as_ref()
        .is_some_and(|(stored_fingerprint, version)| {
            stored_fingerprint.as_slice() == fingerprint && *version == SEARCH_INDEX_VERSION
        })
    {
        return Ok(());
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute("DELETE FROM message_search_terms", [])?;
    transaction.execute("DELETE FROM recipient_search_terms", [])?;
    transaction.execute(
        "UPDATE messages SET search_version = 0, recipient_version = 0
         WHERE search_version != 0 OR recipient_version != 0",
        [],
    )?;
    transaction.execute(
        "INSERT INTO search_index_meta(id, key_fingerprint, index_version)
         VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET
           key_fingerprint = excluded.key_fingerprint,
           index_version = excluded.index_version",
        params![fingerprint, SEARCH_INDEX_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}

fn replace_message_search_terms(
    connection: &Connection,
    account_key: &[u8],
    folder_key: &[u8],
    uid: i64,
    message_terms: &[[u8; 32]],
    recipient_terms: &[[u8; 32]],
) -> Result<()> {
    connection.execute(
        "DELETE FROM message_search_terms
         WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
        params![account_key, folder_key, uid],
    )?;
    {
        let mut insert = connection.prepare(
            "INSERT OR IGNORE INTO message_search_terms(term, account_key, folder_key, uid)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for term in message_terms {
            insert.execute(params![term, account_key, folder_key, uid])?;
        }
    }
    connection.execute(
        "DELETE FROM recipient_search_terms
         WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
        params![account_key, folder_key, uid],
    )?;
    {
        let mut insert = connection.prepare(
            "INSERT OR IGNORE INTO recipient_search_terms(term, account_key, folder_key, uid)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for term in recipient_terms {
            insert.execute(params![term, account_key, folder_key, uid])?;
        }
    }
    connection.execute(
        "UPDATE messages SET search_version = ?4, recipient_version = ?4
         WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
        params![account_key, folder_key, uid, SEARCH_INDEX_VERSION],
    )?;
    Ok(())
}

pub fn rebuild_search_index_batch(
    cache_root: &Path,
    batch_size: usize,
) -> Result<SearchIndexProgress> {
    let key = load_search_key(cache_root)?;
    with_recovery(cache_root, |connection| {
        ensure_search_key_state(connection, &key)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let encrypted_rows = {
            let mut statement = transaction.prepare(
                "SELECT account_key, folder_key, uid, payload
                 FROM messages
                 WHERE search_version != ?1 OR recipient_version != ?1
                 ORDER BY updated_at DESC
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(
                params![
                    SEARCH_INDEX_VERSION,
                    batch_size.clamp(1, SEARCH_INDEX_BATCH_SIZE) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )?;
            let mut values = Vec::new();
            for row in rows {
                values.push(row?);
            }
            values
        };

        for (account_key, folder_key, uid, payload) in &encrypted_rows {
            let terms =
                decrypt_json::<CachedMessage>(payload, "message").and_then(|mut message| {
                    crate::mail::bound_cached_message(&mut message);
                    if identity_key(&message.account_id).as_slice() != account_key
                        || folder_identity_key(&message.account_id, &message.folder).as_slice()
                            != folder_key
                        || i64::from(message.uid) != *uid
                    {
                        return Err(anyhow!("indexed message cache identity mismatch"));
                    }
                    Ok((
                        message_search_terms(&key, &message)?,
                        recipient_search_terms(&key, &message)?,
                    ))
                });
            match terms {
                Ok((message_terms, recipient_terms)) => replace_message_search_terms(
                    &transaction,
                    account_key,
                    folder_key,
                    *uid,
                    &message_terms,
                    &recipient_terms,
                )?,
                Err(error) => {
                    // Search is a secondary index. Mark one unreadable row complete with no terms
                    // so it cannot stall every later batch or prevent healthy mail from being found.
                    tracing::warn!(error = %error, "skipped one unreadable message while rebuilding local search");
                    replace_message_search_terms(
                        &transaction,
                        account_key,
                        folder_key,
                        *uid,
                        &[],
                        &[],
                    )?;
                }
            }
        }
        let has_more: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages
             WHERE search_version != ?1 OR recipient_version != ?1 LIMIT 1)",
            params![SEARCH_INDEX_VERSION],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(SearchIndexProgress {
            indexed: encrypted_rows.len(),
            has_more,
        })
    })
}

pub fn spawn_search_indexer(cache_root: PathBuf) {
    if SEARCH_INDEX_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        SEARCH_INDEX_REQUESTED.store(true, Ordering::Release);
        return;
    }
    let restart_root = cache_root.clone();
    let retry_root = cache_root.clone();
    let spawn = thread::Builder::new()
        .name("mailgo-local-search-index".to_string())
        .spawn(move || {
            {
                let _run = SearchIndexerRun;
                loop {
                    match rebuild_search_index_batch(&cache_root, SEARCH_INDEX_BATCH_SIZE) {
                        Ok(progress) if progress.has_more && progress.indexed > 0 => {
                            thread::yield_now()
                        }
                        Ok(progress) if progress.has_more => {
                            tracing::warn!("local search index made no progress");
                            break;
                        }
                        Ok(_) => break,
                        Err(error) => {
                            tracing::warn!(error = %error, "local search index update paused");
                            break;
                        }
                    }
                }
            }
            if SEARCH_INDEX_REQUESTED.swap(false, Ordering::AcqRel) {
                spawn_search_indexer(restart_root);
            }
        });
    if let Err(error) = spawn {
        SEARCH_INDEX_RUNNING.store(false, Ordering::Release);
        tracing::warn!(error = %error, "could not start local search index worker");
        if SEARCH_INDEX_REQUESTED.swap(false, Ordering::AcqRel) {
            spawn_search_indexer(retry_root);
        }
    }
}

fn rebuild_list_index_batch(cache_root: &Path, batch_size: usize) -> Result<SearchIndexProgress> {
    with_recovery(cache_root, |connection| {
        let encrypted_rows = {
            let mut statement = connection.prepare(
                "SELECT messages.account_key, messages.folder_key, messages.uid, messages.payload
                 FROM messages
                 WHERE NOT EXISTS (
                   SELECT 1 FROM message_list
                   WHERE message_list.account_key = messages.account_key
                     AND message_list.folder_key = messages.folder_key
                     AND message_list.uid = messages.uid
                 )
                 ORDER BY messages.updated_at DESC
                 LIMIT ?1",
            )?;
            let rows = statement.query_map(
                params![batch_size.clamp(1, LIST_INDEX_BATCH_SIZE) as i64],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )?;
            let mut values = Vec::new();
            for row in rows {
                values.push(row?);
            }
            values
        };
        if encrypted_rows.is_empty() {
            return Ok(SearchIndexProgress {
                indexed: 0,
                has_more: false,
            });
        }

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut insert = transaction.prepare(
            "INSERT OR IGNORE INTO message_list(account_key, folder_key, uid, payload)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut inserted_by_mailbox = HashMap::<(Vec<u8>, Vec<u8>), i64>::new();
        for (account_key, folder_key, uid, payload) in &encrypted_rows {
            let list_payload =
                decrypt_json::<CachedMessage>(payload, "message").and_then(|mut message| {
                    crate::mail::bound_cached_message(&mut message);
                    if identity_key(&message.account_id).as_slice() != account_key
                        || folder_identity_key(&message.account_id, &message.folder).as_slice()
                            != folder_key
                        || i64::from(message.uid) != *uid
                    {
                        return Err(anyhow!("indexed message cache identity mismatch"));
                    }
                    encrypt_json(&message_for_list(&message)).map(|(encrypted, _)| encrypted)
                });
            match list_payload {
                Ok(list_payload) => {
                    let inserted =
                        insert.execute(params![account_key, folder_key, uid, list_payload])?;
                    if inserted > 0 {
                        *inserted_by_mailbox
                            .entry((account_key.clone(), folder_key.clone()))
                            .or_default() += inserted as i64;
                    }
                }
                Err(error) => {
                    // A corrupt primary row is handled by the exact-read path. Mark it visited so
                    // one damaged message cannot keep the low-priority migration worker spinning.
                    tracing::warn!(error = %error, "skipped one unreadable message while building list summaries");
                    let inserted =
                        insert.execute(params![account_key, folder_key, uid, payload])?;
                    if inserted > 0 {
                        *inserted_by_mailbox
                            .entry((account_key.clone(), folder_key.clone()))
                            .or_default() += inserted as i64;
                    }
                }
            }
        }
        drop(insert);
        for ((account_key, folder_key), inserted) in inserted_by_mailbox {
            transaction.execute(
                "UPDATE mailbox_meta
                 SET list_count = MIN(message_count, list_count + ?3)
                 WHERE account_key = ?1 AND folder_key = ?2",
                params![account_key, folder_key, inserted],
            )?;
        }
        let has_more: bool = transaction.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM messages
               WHERE NOT EXISTS (
                 SELECT 1 FROM message_list
                 WHERE message_list.account_key = messages.account_key
                   AND message_list.folder_key = messages.folder_key
                   AND message_list.uid = messages.uid
               )
               LIMIT 1
             )",
            [],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(SearchIndexProgress {
            indexed: encrypted_rows.len(),
            has_more,
        })
    })
}

fn spawn_list_indexer(cache_root: PathBuf) {
    if LIST_INDEX_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        LIST_INDEX_REQUESTED.store(true, Ordering::Release);
        return;
    }
    let restart_root = cache_root.clone();
    let retry_root = cache_root.clone();
    let spawn = thread::Builder::new()
        .name("mailgo-list-summary-index".to_string())
        .spawn(move || {
            {
                let _run = ListIndexerRun;
                loop {
                    match rebuild_list_index_batch(&cache_root, LIST_INDEX_BATCH_SIZE) {
                        Ok(progress) if progress.has_more && progress.indexed > 0 => {
                            thread::sleep(Duration::from_millis(12));
                        }
                        Ok(progress) if progress.has_more => {
                            tracing::warn!("mail list summary index made no progress");
                            break;
                        }
                        Ok(_) => break,
                        Err(error) => {
                            tracing::warn!(error = %error, "mail list summary index update paused");
                            break;
                        }
                    }
                }
            }
            if LIST_INDEX_REQUESTED.swap(false, Ordering::AcqRel) {
                spawn_list_indexer(restart_root);
            }
        });
    if let Err(error) = spawn {
        LIST_INDEX_RUNNING.store(false, Ordering::Release);
        tracing::warn!(error = %error, "could not start mail list summary worker");
        if LIST_INDEX_REQUESTED.swap(false, Ordering::AcqRel) {
            spawn_list_indexer(retry_root);
        }
    }
}

#[derive(Clone, Copy)]
enum EncryptedPayloadTable {
    MessageList,
    MailboxMetadata,
    Messages,
}

impl EncryptedPayloadTable {
    fn name(self) -> &'static str {
        match self {
            Self::MessageList => "message_list",
            Self::MailboxMetadata => "mailbox_meta",
            Self::Messages => "messages",
        }
    }

    fn has_uid(self) -> bool {
        !matches!(self, Self::MailboxMetadata)
    }
}

#[derive(Clone)]
struct EncryptionMigrationCursor {
    account_key: Vec<u8>,
    folder_key: Vec<u8>,
    uid: i64,
}

struct EncryptedPayloadRow {
    cursor: EncryptionMigrationCursor,
    payload: Vec<u8>,
}

struct EncryptedPayloadUpdate {
    row: EncryptedPayloadRow,
    encrypted: Vec<u8>,
}

fn read_encrypted_payload_batch(
    cache_root: &Path,
    table: EncryptedPayloadTable,
    cursor: Option<&EncryptionMigrationCursor>,
) -> Result<Vec<EncryptedPayloadRow>> {
    with_recovery(cache_root, |connection| {
        let (uid_expression, ordering) = if table.has_uid() {
            ("uid", "account_key, folder_key, uid")
        } else {
            ("0", "account_key, folder_key")
        };
        let sql = format!(
            "SELECT account_key, folder_key, {uid_expression}, payload
             FROM {}
             WHERE encryption_version = 0
               AND (?1 IS NULL
                    OR (account_key, folder_key, {uid_expression}) > (?2, ?3, ?4))
             ORDER BY {ordering}
             LIMIT ?5",
            table.name()
        );
        let present = cursor.map(|_| 1_i64);
        let account_key = cursor.map_or(&[][..], |value| value.account_key.as_slice());
        let folder_key = cursor.map_or(&[][..], |value| value.folder_key.as_slice());
        let uid = cursor.map_or(0_i64, |value| value.uid);
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(
            params![
                present,
                account_key,
                folder_key,
                uid,
                ENCRYPTION_MIGRATION_BATCH_SIZE as i64
            ],
            |row| {
                Ok(EncryptedPayloadRow {
                    cursor: EncryptionMigrationCursor {
                        account_key: row.get(0)?,
                        folder_key: row.get(1)?,
                        uid: row.get(2)?,
                    },
                    payload: row.get(3)?,
                })
            },
        )?;
        let mut values = Vec::with_capacity(ENCRYPTION_MIGRATION_BATCH_SIZE);
        let mut payload_bytes = 0usize;
        for row in rows {
            let row = row?;
            payload_bytes = payload_bytes.saturating_add(row.payload.len());
            values.push(row);
            if payload_bytes >= ENCRYPTION_MIGRATION_BATCH_BYTES {
                break;
            }
        }
        Ok(values)
    })
}

fn migrate_encrypted_payload_rows(
    cache_root: &Path,
    table: EncryptedPayloadTable,
    rows: Vec<EncryptedPayloadRow>,
) -> Result<(usize, usize)> {
    let mut skipped = 0usize;
    let mut updates = Vec::new();
    for row in rows {
        if crate::sync::database_cache_uses_current_envelope(&row.payload) {
            updates.push(EncryptedPayloadUpdate {
                encrypted: row.payload.clone(),
                row,
            });
            continue;
        }
        if row.payload.len() > MAX_ENCRYPTED_ROW_BYTES {
            skipped = skipped.saturating_add(1);
            continue;
        }
        let encrypted = crate::sync::unprotect_database_cache(&row.payload)
            .and_then(|plaintext| crate::sync::protect_database_cache(&plaintext));
        match encrypted {
            Ok(encrypted) => updates.push(EncryptedPayloadUpdate { row, encrypted }),
            Err(error) => {
                skipped = skipped.saturating_add(1);
                tracing::warn!(
                    table = table.name(),
                    error = %error,
                    "skipped one unreadable legacy cache row during encryption migration"
                );
            }
        }
    }
    if updates.is_empty() {
        return Ok((0, skipped));
    }

    let migrated = with_recovery(cache_root, |connection| {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut changed = 0usize;
        if table.has_uid() {
            let sql = format!(
                "UPDATE {} SET payload = ?4, encryption_version = ?6
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3
                   AND payload = ?5 AND encryption_version = 0",
                table.name()
            );
            let mut update = transaction.prepare(&sql)?;
            for value in &updates {
                changed = changed.saturating_add(update.execute(params![
                    value.row.cursor.account_key,
                    value.row.cursor.folder_key,
                    value.row.cursor.uid,
                    value.encrypted,
                    value.row.payload,
                    CURRENT_DATABASE_ENCRYPTION_VERSION
                ])?);
            }
        } else {
            let sql = format!(
                "UPDATE {} SET payload = ?3, encryption_version = ?5
                 WHERE account_key = ?1 AND folder_key = ?2
                   AND payload = ?4 AND encryption_version = 0",
                table.name()
            );
            let mut update = transaction.prepare(&sql)?;
            for value in &updates {
                changed = changed.saturating_add(update.execute(params![
                    value.row.cursor.account_key,
                    value.row.cursor.folder_key,
                    value.encrypted,
                    value.row.payload,
                    CURRENT_DATABASE_ENCRYPTION_VERSION
                ])?);
            }
        }
        transaction.commit()?;
        Ok(changed)
    })?;
    Ok((migrated, skipped))
}

fn migrate_encrypted_payload_table(
    cache_root: &Path,
    table: EncryptedPayloadTable,
) -> Result<(usize, usize)> {
    let mut cursor = None;
    let mut migrated = 0usize;
    let mut skipped = 0usize;
    loop {
        let rows = read_encrypted_payload_batch(cache_root, table, cursor.as_ref())?;
        let Some(next_cursor) = rows.last().map(|row| row.cursor.clone()) else {
            break;
        };
        let (batch_migrated, batch_skipped) =
            migrate_encrypted_payload_rows(cache_root, table, rows)?;
        migrated = migrated.saturating_add(batch_migrated);
        skipped = skipped.saturating_add(batch_skipped);
        cursor = Some(next_cursor);
        thread::sleep(Duration::from_millis(8));
    }
    Ok((migrated, skipped))
}

/// Re-encrypt legacy Windows DPAPI-per-row payloads through the current AEAD envelope. The worker
/// waits for the local-first renderer to hydrate, scans each table in primary-key order with a
/// bounded memory budget, and conditionally updates exact ciphertext so concurrent sync writes win.
pub fn spawn_encryption_migrator(cache_root: PathBuf) {
    if ENCRYPTION_MIGRATION_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let spawn = thread::Builder::new()
        .name("mailgo-cache-encryption-migration".to_string())
        .spawn(move || {
            let _run = EncryptionMigrationRun;
            thread::sleep(Duration::from_secs(1));
            let mut migrated = 0usize;
            let mut skipped = 0usize;
            for table in [
                EncryptedPayloadTable::MessageList,
                EncryptedPayloadTable::MailboxMetadata,
                EncryptedPayloadTable::Messages,
            ] {
                match migrate_encrypted_payload_table(&cache_root, table) {
                    Ok((table_migrated, table_skipped)) => {
                        migrated = migrated.saturating_add(table_migrated);
                        skipped = skipped.saturating_add(table_skipped);
                    }
                    Err(error) => {
                        tracing::warn!(
                            table = table.name(),
                            error = %error,
                            "background cache encryption migration paused"
                        );
                        return;
                    }
                }
            }
            tracing::info!(
                migrated_rows = migrated,
                skipped_rows = skipped,
                "background cache encryption migration completed"
            );
        });
    if let Err(error) = spawn {
        ENCRYPTION_MIGRATION_RUNNING.store(false, Ordering::Release);
        tracing::warn!(error = %error, "could not start cache encryption migration worker");
    }
}

fn search_match_score(message: &CachedMessage, query_words: &[String]) -> Option<u16> {
    let all_match = |fields: &[&str]| {
        let words = normalize_search_words(fields.iter().copied());
        query_words
            .iter()
            .all(|query| words.iter().any(|word| word.contains(query)))
    };
    let mut score = 0u16;
    if all_match(&[&message.subject]) {
        score += 100;
    }
    if all_match(&[&message.sender_name, &message.sender_email]) {
        score += 60;
    }
    let recipients = message
        .to
        .iter()
        .chain(message.cc.iter())
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !recipients.is_empty() && all_match(&recipients) {
        score += 30;
    }
    if all_match(&[&message.preview]) {
        score += 20;
    }
    if all_match(&[&message.text_body]) {
        score += 10;
    }
    let every_field = message_search_words(message);
    if !query_words
        .iter()
        .all(|query| every_field.iter().any(|word| word.contains(query)))
    {
        return None;
    }
    Some(score.max(1))
}

pub fn search_messages(
    cache_root: &Path,
    account_ids: &[String],
    query: &str,
    limit: usize,
) -> Result<LocalSearchResult> {
    let query_words = query_search_words(query);
    if account_ids.is_empty() || query_words.is_empty() {
        return Ok(LocalSearchResult {
            messages: Vec::new(),
            truncated: false,
            indexing: false,
        });
    }
    let key = load_search_key(cache_root)?;
    let progress = if SEARCH_INDEX_RUNNING.load(Ordering::Acquire) {
        SearchIndexProgress {
            indexed: 0,
            has_more: true,
        }
    } else {
        rebuild_search_index_batch(cache_root, SEARCH_INDEX_FOREGROUND_BATCH_SIZE)?
    };
    let query_terms = query_search_terms(&key, &query_words)?;
    if query_terms.is_empty() {
        return Ok(LocalSearchResult {
            messages: Vec::new(),
            truncated: false,
            indexing: progress.has_more,
        });
    }

    let account_directory = account_ids
        .iter()
        .map(|account_id| (identity_key(account_id), account_id.as_str()))
        .collect::<HashMap<_, _>>();
    let bounded_limit = limit.clamp(1, MAX_PAGE_SIZE);
    let mut result = with_recovery(cache_root, |connection| {
        ensure_search_key_state(connection, &key)?;
        let term_placeholders = (1..=query_terms.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let account_offset = query_terms.len();
        let account_placeholders = (1..=account_directory.len())
            .map(|index| format!("?{}", account_offset + index))
            .collect::<Vec<_>>()
            .join(", ");
        let count_parameter = account_offset + account_directory.len() + 1;
        let limit_parameter = count_parameter + 1;
        let sql = format!(
            "SELECT messages.account_key, messages.folder_key, messages.uid, messages.payload
             FROM messages
             INNER JOIN (
               SELECT account_key, folder_key, uid
               FROM message_search_terms
               WHERE term IN ({term_placeholders})
                 AND account_key IN ({account_placeholders})
               GROUP BY account_key, folder_key, uid
               HAVING COUNT(DISTINCT term) = ?{count_parameter}
               LIMIT ?{limit_parameter}
             ) AS candidates
             ON candidates.account_key = messages.account_key
               AND candidates.folder_key = messages.folder_key
               AND candidates.uid = messages.uid
             ORDER BY messages.updated_at DESC"
        );
        let mut parameters = query_terms
            .iter()
            .map(|term| SqlValue::Blob(term.to_vec()))
            .collect::<Vec<_>>();
        parameters.extend(
            account_directory
                .keys()
                .map(|account_key| SqlValue::Blob(account_key.to_vec())),
        );
        parameters.push(SqlValue::Integer(query_terms.len() as i64));
        parameters.push(SqlValue::Integer((MAX_LOCAL_SEARCH_CANDIDATES + 1) as i64));
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut ranked = Vec::new();
        let mut candidate_count = 0usize;
        for row in rows {
            candidate_count += 1;
            if candidate_count > MAX_LOCAL_SEARCH_CANDIDATES {
                break;
            }
            let (stored_account_key, stored_folder_key, uid, payload) = row?;
            let Ok(account_key) = <[u8; 32]>::try_from(stored_account_key.as_slice()) else {
                continue;
            };
            let Some(account_id) = account_directory.get(&account_key) else {
                continue;
            };
            let Ok(mut message) = decrypt_json::<CachedMessage>(&payload, "search candidate")
            else {
                continue;
            };
            crate::mail::bound_cached_message(&mut message);
            if message.account_id != **account_id
                || folder_identity_key(&message.account_id, &message.folder).as_slice()
                    != stored_folder_key
                || i64::from(message.uid) != uid
            {
                continue;
            }
            if let Some(score) = search_match_score(&message, &query_words) {
                ranked.push((score, message));
            }
        }
        ranked.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| right.received_at.cmp(&left.received_at))
                .then_with(|| right.uid.cmp(&left.uid))
        });
        let truncated =
            candidate_count > MAX_LOCAL_SEARCH_CANDIDATES || ranked.len() > bounded_limit;
        ranked.truncate(bounded_limit);
        Ok(LocalSearchResult {
            messages: ranked.into_iter().map(|(_, message)| message).collect(),
            truncated,
            indexing: progress.has_more,
        })
    })?;
    if result.indexing {
        spawn_search_indexer(cache_root.to_path_buf());
    }
    result.indexing |= SEARCH_INDEX_RUNNING.load(Ordering::Acquire);
    Ok(result)
}

#[derive(Debug)]
struct RecipientAggregate {
    suggestion: RecipientSuggestion,
    score: u16,
}

fn safe_recipient_name(value: &str, email: &str) -> String {
    let mut output = String::new();
    let mut pending_space = false;
    for character in value.trim().chars() {
        if character.is_control()
            || matches!(
                character,
                '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{2069}'
                    | '\u{feff}'
                    | '<'
                    | '>'
                    | ','
                    | ';'
                    | '"'
                    | '\\'
            )
        {
            pending_space = !output.is_empty();
            continue;
        }
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space && output.len() < 160 {
            output.push(' ');
            pending_space = false;
        }
        if output.len() + character.len_utf8() > 160 {
            break;
        }
        output.push(character);
    }
    let output = output.trim().to_string();
    if output.eq_ignore_ascii_case(email) {
        String::new()
    } else {
        output
    }
}

fn recipient_match_score(name: &str, email: &str, query_words: &[String]) -> Option<u16> {
    let searchable = normalize_search_words([name, email]);
    if !query_words
        .iter()
        .all(|query| searchable.iter().any(|word| word.contains(query)))
    {
        return None;
    }
    let email_lower = email.to_lowercase();
    let name_lower = name.to_lowercase();
    let raw_query = query_words.join(" ");
    let first = query_words.first().map(String::as_str).unwrap_or_default();
    let score = if email_lower == raw_query {
        1_000
    } else if email_lower.starts_with(&raw_query) {
        850
    } else if email_lower
        .split_once('@')
        .is_some_and(|(local, _)| local.starts_with(first))
    {
        760
    } else if name_lower.starts_with(&raw_query) || name_lower.starts_with(first) {
        680
    } else if email_lower.contains(&raw_query) {
        520
    } else if name_lower.contains(&raw_query) {
        440
    } else {
        300
    };
    Some(score)
}

fn collect_recipient_candidate(
    aggregates: &mut HashMap<String, RecipientAggregate>,
    seen_in_message: &mut HashSet<String>,
    name: &str,
    email: &str,
    own_email: &str,
    query_words: &[String],
    received_at: Option<&str>,
) {
    let email = email.trim();
    if email.eq_ignore_ascii_case(own_email) || crate::providers::validate_email(email).is_err() {
        return;
    }
    let normalized_email = email.to_lowercase();
    if !seen_in_message.insert(normalized_email.clone()) {
        return;
    }
    let name = safe_recipient_name(name, email);
    let Some(score) = recipient_match_score(&name, email, query_words) else {
        return;
    };
    let entry = aggregates
        .entry(normalized_email)
        .or_insert_with(|| RecipientAggregate {
            suggestion: RecipientSuggestion {
                name: name.clone(),
                email: email.to_string(),
                frequency: 0,
                last_seen: received_at.map(str::to_string),
            },
            score,
        });
    entry.suggestion.frequency = entry.suggestion.frequency.saturating_add(1);
    entry.score = entry.score.max(score);
    if entry.suggestion.name.is_empty() && !name.is_empty() {
        entry.suggestion.name = name;
    }
    if received_at.is_some_and(|date| {
        entry
            .suggestion
            .last_seen
            .as_deref()
            .is_none_or(|stored| date > stored)
    }) {
        entry.suggestion.last_seen = received_at.map(str::to_string);
        entry.suggestion.email = email.to_string();
    }
}

pub fn suggest_recipients(
    cache_root: &Path,
    account_id: &str,
    own_email: &str,
    query: &str,
    limit: usize,
) -> Result<RecipientSuggestionResult> {
    let query_words = query_search_words(query);
    if query_words.is_empty() {
        return Ok(RecipientSuggestionResult {
            suggestions: Vec::new(),
            truncated: false,
            indexing: false,
        });
    }
    let key = load_search_key(cache_root)?;
    let query_terms = recipient_query_terms(&key, &query_words)?;
    let account_key = identity_key(account_id);
    let bounded_limit = limit.clamp(1, MAX_RECIPIENT_SUGGESTIONS);
    let (encrypted_rows, candidate_truncated, needs_indexing, needs_list_index) =
        with_recovery(cache_root, |connection| {
            ensure_search_key_state(connection, &key)?;
            let needs_indexing: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages
                 WHERE account_key = ?1 AND recipient_version != ?2 LIMIT 1)",
                params![account_key, SEARCH_INDEX_VERSION],
                |row| row.get(0),
            )?;
            let needs_list_index: bool = connection.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM messages
                   WHERE account_key = ?1 AND NOT EXISTS (
                     SELECT 1 FROM message_list
                     WHERE message_list.account_key = messages.account_key
                       AND message_list.folder_key = messages.folder_key
                       AND message_list.uid = messages.uid
                   ) LIMIT 1
                 )",
                params![account_key],
                |row| row.get(0),
            )?;
            let mut encrypted_rows: Vec<(Vec<u8>, i64, Vec<u8>)> = Vec::new();
            let mut seen_rows: HashSet<(Vec<u8>, i64)> = HashSet::new();
            let mut candidate_truncated = false;

            if !query_terms.is_empty() {
                let term_placeholders = (1..=query_terms.len())
                    .map(|index| format!("?{index}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let account_parameter = query_terms.len() + 1;
                let count_parameter = account_parameter + 1;
                let limit_parameter = count_parameter + 1;
                let sql = format!(
                    "SELECT message_list.folder_key, message_list.uid, message_list.payload
                     FROM message_list
                     INNER JOIN messages USING(account_key, folder_key, uid)
                     INNER JOIN (
                       SELECT account_key, folder_key, uid
                       FROM recipient_search_terms
                       WHERE term IN ({term_placeholders}) AND account_key = ?{account_parameter}
                       GROUP BY account_key, folder_key, uid
                       HAVING COUNT(DISTINCT term) = ?{count_parameter}
                     ) AS candidates USING(account_key, folder_key, uid)
                     ORDER BY messages.updated_at DESC, messages.uid DESC
                     LIMIT ?{limit_parameter}"
                );
                let mut parameters = query_terms
                    .iter()
                    .map(|term| SqlValue::Blob(term.to_vec()))
                    .collect::<Vec<_>>();
                parameters.push(SqlValue::Blob(account_key.to_vec()));
                parameters.push(SqlValue::Integer(query_terms.len() as i64));
                parameters.push(SqlValue::Integer(
                    (MAX_RECIPIENT_INDEX_CANDIDATES + 1) as i64,
                ));
                let mut statement = connection.prepare(&sql)?;
                let rows =
                    statement.query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                        ))
                    })?;
                for row in rows {
                    let row = row?;
                    if encrypted_rows.len() == MAX_RECIPIENT_INDEX_CANDIDATES {
                        candidate_truncated = true;
                        break;
                    }
                    seen_rows.insert((row.0.clone(), row.1));
                    encrypted_rows.push(row);
                }
            }

            let recent_limit = if query_terms.is_empty() {
                MAX_RECIPIENT_FALLBACK_CANDIDATES
            } else {
                MAX_RECIPIENT_RECENT_CANDIDATES
            };
            let mut statement = connection.prepare(
                "SELECT message_list.folder_key, message_list.uid, message_list.payload
                 FROM message_list INNER JOIN messages USING(account_key, folder_key, uid)
                 WHERE message_list.account_key = ?1
                 ORDER BY messages.updated_at DESC, messages.uid DESC
                 LIMIT ?2",
            )?;
            let rows =
                statement.query_map(params![account_key, (recent_limit + 1) as i64], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })?;
            let mut recent_count = 0usize;
            for row in rows {
                recent_count += 1;
                if recent_count > recent_limit {
                    candidate_truncated = true;
                    break;
                }
                let row = row?;
                if seen_rows.insert((row.0.clone(), row.1)) {
                    encrypted_rows.push(row);
                }
            }
            Ok((
                encrypted_rows,
                candidate_truncated,
                needs_indexing,
                needs_list_index,
            ))
        })?;

    let mut aggregates = HashMap::new();
    for (stored_folder_key, uid, payload) in encrypted_rows {
        let Ok(mut message) = decrypt_json::<CachedMessage>(&payload, "recipient suggestion")
        else {
            continue;
        };
        crate::mail::bound_cached_message(&mut message);
        if message.account_id != account_id
            || folder_identity_key(&message.account_id, &message.folder).as_slice()
                != stored_folder_key
            || i64::from(message.uid) != uid
        {
            continue;
        }
        let mut seen_in_message = HashSet::new();
        collect_recipient_candidate(
            &mut aggregates,
            &mut seen_in_message,
            &message.sender_name,
            &message.sender_email,
            own_email,
            &query_words,
            message.received_at.as_deref(),
        );
        for email in message.to.iter().chain(message.cc.iter()) {
            collect_recipient_candidate(
                &mut aggregates,
                &mut seen_in_message,
                "",
                email,
                own_email,
                &query_words,
                message.received_at.as_deref(),
            );
        }
    }
    let mut ranked = aggregates.into_values().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.suggestion.frequency.cmp(&left.suggestion.frequency))
            .then_with(|| right.suggestion.last_seen.cmp(&left.suggestion.last_seen))
            .then_with(|| left.suggestion.email.cmp(&right.suggestion.email))
    });
    let truncated = candidate_truncated || ranked.len() > bounded_limit;
    ranked.truncate(bounded_limit);
    if needs_indexing {
        spawn_search_indexer(cache_root.to_path_buf());
    }
    if needs_list_index {
        spawn_list_indexer(cache_root.to_path_buf());
    }
    Ok(RecipientSuggestionResult {
        suggestions: ranked
            .into_iter()
            .map(|aggregate| aggregate.suggestion)
            .collect(),
        truncated,
        indexing: needs_indexing
            || needs_list_index
            || SEARCH_INDEX_RUNNING.load(Ordering::Acquire)
            || LIST_INDEX_RUNNING.load(Ordering::Acquire),
    })
}

pub fn load_mailbox(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
) -> Result<Option<CachedMailbox>> {
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let Some((metadata, _)) = read_metadata(connection, &account_key, &folder_key)? else {
            return Ok(None);
        };
        validate_identity(&metadata, account_id, folder)?;
        let mut statement = connection.prepare(
            "SELECT uid, payload FROM messages
             WHERE account_key = ?1 AND folder_key = ?2
             ORDER BY uid DESC",
        )?;
        let rows = statement.query_map(params![account_key, folder_key], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (uid, payload) = row?;
            let uid = u32::try_from(uid).context("indexed message UID is out of range")?;
            messages.push(decrypt_message(&payload, account_id, folder, uid)?);
        }
        Ok(Some(metadata.into_mailbox(messages)))
    })
}

/// Load the bounded, body-free mailbox state used by background synchronization. Modern caches
/// read only `message_list`; an incomplete schema-v3 backfill may temporarily fall back to the
/// corresponding full row for a missing summary, without scanning beyond the requested window.
pub fn load_mailbox_summaries(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    limit: usize,
) -> Result<Option<CachedMailbox>> {
    let (mailbox, needs_list_index) = with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let Some((metadata, _, total_cached, summaries_complete)) =
            read_metadata_for_page(connection, &account_key, &folder_key)?
        else {
            return Ok((None, false));
        };
        validate_identity(&metadata, account_id, folder)?;
        let bounded_limit = limit.clamp(1, MAX_SYNC_SUMMARIES);
        let encrypted_rows = if summaries_complete {
            let mut statement = connection.prepare(
                "SELECT uid, payload FROM message_list
                 WHERE account_key = ?1 AND folder_key = ?2
                 ORDER BY uid DESC LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![account_key, folder_key, bounded_limit as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )?;
            let mut values = Vec::with_capacity(bounded_limit.min(total_cached));
            for row in rows {
                values.push(row?);
            }
            values
        } else {
            let mut statement = connection.prepare(
                "SELECT messages.uid, COALESCE(message_list.payload, messages.payload)
                 FROM messages LEFT JOIN message_list
                   ON message_list.account_key = messages.account_key
                  AND message_list.folder_key = messages.folder_key
                  AND message_list.uid = messages.uid
                 WHERE messages.account_key = ?1 AND messages.folder_key = ?2
                 ORDER BY messages.uid DESC LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![account_key, folder_key, bounded_limit as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )?;
            let mut values = Vec::with_capacity(bounded_limit.min(total_cached));
            for row in rows {
                values.push(row?);
            }
            values
        };
        let mut messages = Vec::with_capacity(encrypted_rows.len());
        for (uid, payload) in encrypted_rows {
            let uid = u32::try_from(uid).context("indexed message UID is out of range")?;
            messages.push(decrypt_list_message(&payload, account_id, folder, uid)?);
        }
        let loaded_all = messages.len() >= total_cached;
        let mut mailbox = metadata.into_mailbox(messages);
        mailbox.has_more |= !loaded_all;
        Ok((Some(mailbox), !summaries_complete))
    })?;
    if needs_list_index {
        spawn_list_indexer(cache_root.to_path_buf());
    }
    Ok(mailbox)
}

pub fn mailbox_exists(cache_root: &Path, account_id: &str, folder: &str) -> Result<bool> {
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let Some((metadata, _)) = read_metadata(connection, &account_key, &folder_key)? else {
            return Ok(false);
        };
        validate_identity(&metadata, account_id, folder)?;
        Ok(true)
    })
}

/// Read only the encrypted mailbox metadata row so renderer polling can avoid decrypting and
/// serializing an unchanged page of message summaries. Identity validation is deliberately kept
/// on this fast path rather than trusting only the keyed database lookup.
pub fn mailbox_revision(cache_root: &Path, account_id: &str, folder: &str) -> Result<Option<u64>> {
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let Some((metadata, revision)) = read_metadata(connection, &account_key, &folder_key)?
        else {
            return Ok(None);
        };
        validate_identity(&metadata, account_id, folder)?;
        Ok(Some(revision))
    })
}

pub fn load_mailbox_page(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    before_uid: Option<u32>,
    limit: usize,
) -> Result<Option<MailboxPage>> {
    let (page, needs_list_index) = with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let Some((metadata, revision, total_cached, summaries_complete)) =
            read_metadata_for_page(connection, &account_key, &folder_key)?
        else {
            return Ok((None, false));
        };
        validate_identity(&metadata, account_id, folder)?;
        let page_size = limit.clamp(1, MAX_PAGE_SIZE);
        let encrypted_rows = match (summaries_complete, before_uid) {
            (true, Some(before_uid)) => {
                let mut statement = connection.prepare(
                    "SELECT uid, payload FROM message_list
                     WHERE account_key = ?1 AND folder_key = ?2 AND uid < ?3
                     ORDER BY uid DESC LIMIT ?4",
                )?;
                let rows = statement.query_map(
                    params![
                        account_key,
                        folder_key,
                        i64::from(before_uid),
                        (page_size + 1) as i64
                    ],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )?;
                let mut values = Vec::with_capacity(page_size + 1);
                for row in rows {
                    values.push(row?);
                }
                values
            }
            (true, None) => {
                let mut statement = connection.prepare(
                    "SELECT uid, payload FROM message_list
                     WHERE account_key = ?1 AND folder_key = ?2
                     ORDER BY uid DESC LIMIT ?3",
                )?;
                let rows = statement.query_map(
                    params![account_key, folder_key, (page_size + 1) as i64],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )?;
                let mut values = Vec::with_capacity(page_size + 1);
                for row in rows {
                    values.push(row?);
                }
                values
            }
            (false, Some(before_uid)) => {
                let mut statement = connection.prepare(
                    "SELECT messages.uid, COALESCE(message_list.payload, messages.payload)
                     FROM messages LEFT JOIN message_list
                       ON message_list.account_key = messages.account_key
                      AND message_list.folder_key = messages.folder_key
                      AND message_list.uid = messages.uid
                     WHERE messages.account_key = ?1 AND messages.folder_key = ?2
                       AND messages.uid < ?3
                     ORDER BY messages.uid DESC LIMIT ?4",
                )?;
                let rows = statement.query_map(
                    params![
                        account_key,
                        folder_key,
                        i64::from(before_uid),
                        (page_size + 1) as i64
                    ],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )?;
                let mut values = Vec::with_capacity(page_size + 1);
                for row in rows {
                    values.push(row?);
                }
                values
            }
            (false, None) => {
                let mut statement = connection.prepare(
                    "SELECT messages.uid, COALESCE(message_list.payload, messages.payload)
                     FROM messages LEFT JOIN message_list
                       ON message_list.account_key = messages.account_key
                      AND message_list.folder_key = messages.folder_key
                      AND message_list.uid = messages.uid
                     WHERE messages.account_key = ?1 AND messages.folder_key = ?2
                     ORDER BY messages.uid DESC LIMIT ?3",
                )?;
                let rows = statement.query_map(
                    params![account_key, folder_key, (page_size + 1) as i64],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )?;
                let mut values = Vec::with_capacity(page_size + 1);
                for row in rows {
                    values.push(row?);
                }
                values
            }
        };
        let mut encrypted_rows = encrypted_rows;
        let local_has_more = encrypted_rows.len() > page_size;
        encrypted_rows.truncate(page_size);
        let mut messages = Vec::with_capacity(encrypted_rows.len());
        for (uid, payload) in encrypted_rows {
            let uid = u32::try_from(uid).context("indexed message UID is out of range")?;
            messages.push(decrypt_list_message(&payload, account_id, folder, uid)?);
        }
        let remote_has_more = metadata.has_more;
        let needs_list_index = !summaries_complete;
        let mut mailbox = metadata.into_mailbox(messages);
        mailbox.oldest_uid = mailbox.messages.iter().map(|message| message.uid).min();
        mailbox.has_more = local_has_more || remote_has_more;
        Ok((
            Some(MailboxPage {
                mailbox,
                local_has_more,
                remote_has_more,
                total_cached,
                revision,
            }),
            needs_list_index,
        ))
    })?;
    if needs_list_index {
        spawn_list_indexer(cache_root.to_path_buf());
    }
    Ok(page)
}

pub fn load_message(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<Option<CachedMessage>> {
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let payload = connection
            .query_row(
                "SELECT payload FROM messages
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
                params![account_key, folder_key, i64::from(uid)],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        payload
            .map(|payload| decrypt_message(&payload, account_id, folder, uid))
            .transpose()
    })
}

pub fn save_mailbox(
    cache_root: &Path,
    account_id: &str,
    mailbox: &CachedMailbox,
) -> Result<PathBuf> {
    if mailbox.account_id != account_id {
        return Err(anyhow!("indexed mail cache account mismatch"));
    }
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, &mailbox.folder);
        let now = now_epoch_millis();
        let current_uids = mailbox
            .messages
            .iter()
            .map(|message| i64::from(message.uid))
            .collect::<HashSet<_>>();
        let (metadata_payload, _) = encrypt_json(&MailboxMetadata::from(mailbox))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO mailbox_meta(account_key, folder_key, payload, revision, message_count, list_count, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?4, ?5)
             ON CONFLICT(account_key, folder_key) DO UPDATE SET
               payload = excluded.payload,
               revision = mailbox_meta.revision + 1,
               message_count = excluded.message_count,
               list_count = excluded.list_count,
               updated_at = excluded.updated_at",
            params![
                account_key,
                folder_key,
                metadata_payload,
                current_uids.len() as i64,
                now
            ],
        )?;

        let existing = {
            let mut statement = transaction.prepare(
                "SELECT uid, payload_hash FROM messages
                 WHERE account_key = ?1 AND folder_key = ?2",
            )?;
            let rows = statement.query_map(params![account_key, folder_key], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            let mut result = HashMap::new();
            for row in rows {
                let (uid, digest) = row?;
                result.insert(uid, digest);
            }
            result
        };
        {
            let mut delete = transaction.prepare(
                "DELETE FROM messages WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
            )?;
            for uid in existing.keys().filter(|uid| !current_uids.contains(uid)) {
                delete.execute(params![account_key, folder_key, uid])?;
            }
        }
        {
            let mut upsert_message = transaction.prepare(
                "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, recipient_version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload_hash = excluded.payload_hash,
                   payload = excluded.payload,
                   search_version = 0,
                   recipient_version = 0,
                   updated_at = excluded.updated_at",
            )?;
            let mut upsert_list = transaction.prepare(
                "INSERT INTO message_list(account_key, folder_key, uid, payload)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload = excluded.payload",
            )?;
            for message in &mailbox.messages {
                if message.account_id != account_id
                    || !message.folder.eq_ignore_ascii_case(&mailbox.folder)
                {
                    return Err(anyhow!("indexed message cache identity mismatch"));
                }
                let (payload, digest, list_payload) = encrypt_message_payloads(message)?;
                let uid = i64::from(message.uid);
                if !existing
                    .get(&uid)
                    .is_some_and(|stored| stored.as_slice() == &digest[..])
                {
                    upsert_message.execute(params![
                        account_key,
                        folder_key,
                        uid,
                        digest,
                        payload,
                        now
                    ])?;
                }
                upsert_list.execute(params![account_key, folder_key, uid, list_payload])?;
            }
        }
        transaction.commit()?;
        Ok(())
    })?;
    Ok(database_path(cache_root))
}

pub fn save_message(cache_root: &Path, account_id: &str, message: &CachedMessage) -> Result<()> {
    if message.account_id != account_id {
        return Err(anyhow!("indexed mail cache account mismatch"));
    }
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, &message.folder);
        let now = now_epoch_millis();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (message_existed, list_existed): (bool, bool) = transaction.query_row(
            "SELECT
               EXISTS(SELECT 1 FROM messages
                      WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3),
               EXISTS(SELECT 1 FROM message_list
                      WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3)",
            params![account_key, folder_key, i64::from(message.uid)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let existing_metadata = read_metadata(&transaction, &account_key, &folder_key)?;
        let mut metadata = existing_metadata
            .as_ref()
            .map(|(metadata, _)| metadata.clone())
            .unwrap_or_else(|| MailboxMetadata {
                schema_version: CACHE_SCHEMA_VERSION,
                account_id: account_id.to_string(),
                folder: message.folder.clone(),
                uid_validity: None,
                highest_mod_seq: None,
                synced_at: String::new(),
                oldest_uid: Some(message.uid),
                has_more: false,
            });
        validate_identity(&metadata, account_id, &message.folder)?;
        metadata.oldest_uid = Some(
            metadata
                .oldest_uid
                .map_or(message.uid, |oldest| oldest.min(message.uid)),
        );
        metadata.synced_at = format!("unix:{}", now / 1000);
        let (metadata_payload, _) = encrypt_json(&metadata)?;
        transaction.execute(
            "INSERT INTO mailbox_meta(account_key, folder_key, payload, revision, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(account_key, folder_key) DO UPDATE SET
               payload = excluded.payload,
               revision = mailbox_meta.revision + 1,
               updated_at = excluded.updated_at",
            params![account_key, folder_key, metadata_payload, now],
        )?;
        let (payload, digest, list_payload) = encrypt_message_payloads(message)?;
        transaction.execute(
            "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, recipient_version, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)
             ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
               payload_hash = excluded.payload_hash,
               payload = excluded.payload,
               search_version = 0,
               recipient_version = 0,
               updated_at = excluded.updated_at",
            params![
                account_key,
                folder_key,
                i64::from(message.uid),
                digest,
                payload,
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO message_list(account_key, folder_key, uid, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
               payload = excluded.payload",
            params![
                account_key,
                folder_key,
                i64::from(message.uid),
                list_payload
            ],
        )?;
        transaction.execute(
            "UPDATE mailbox_meta SET
               message_count = message_count + ?3,
               list_count = list_count + ?4
             WHERE account_key = ?1 AND folder_key = ?2",
            params![
                account_key,
                folder_key,
                i64::from(!message_existed),
                i64::from(!list_existed)
            ],
        )?;
        transaction.commit()?;
        Ok(())
    })
}

pub fn merge_messages(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid_validity: Option<u32>,
    synced_at: &str,
    messages: &[CachedMessage],
    max_messages: usize,
) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    if messages.iter().any(|message| {
        message.account_id != account_id || !message.folder.eq_ignore_ascii_case(folder)
    }) {
        return Err(anyhow!("indexed message cache identity mismatch"));
    }
    let max_messages = i64::try_from(max_messages.max(1)).unwrap_or(i64::MAX);
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let now = now_epoch_millis();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read_metadata(&transaction, &account_key, &folder_key)?;
        let mut metadata = existing
            .as_ref()
            .map(|(metadata, _)| metadata.clone())
            .unwrap_or_else(|| MailboxMetadata {
                schema_version: CACHE_SCHEMA_VERSION,
                account_id: account_id.to_string(),
                folder: folder.to_string(),
                uid_validity,
                highest_mod_seq: None,
                synced_at: synced_at.to_string(),
                oldest_uid: None,
                has_more: false,
            });
        validate_identity(&metadata, account_id, folder)?;
        let uid_space_changed =
            metadata.uid_validity.is_some() && metadata.uid_validity != uid_validity;
        if uid_space_changed {
            transaction.execute(
                "DELETE FROM messages WHERE account_key = ?1 AND folder_key = ?2",
                params![account_key, folder_key],
            )?;
            metadata.highest_mod_seq = None;
            metadata.oldest_uid = None;
            metadata.has_more = false;
        }
        metadata.uid_validity = uid_validity;
        metadata.synced_at = synced_at.to_string();

        if existing.is_none() {
            let (metadata_payload, _) = encrypt_json(&metadata)?;
            transaction.execute(
                "INSERT INTO mailbox_meta(account_key, folder_key, payload, revision, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![account_key, folder_key, metadata_payload, now],
            )?;
        }
        {
            let mut upsert_message = transaction.prepare(
                "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, recipient_version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload_hash = excluded.payload_hash,
                   payload = excluded.payload,
                   search_version = 0,
                   recipient_version = 0,
                   updated_at = excluded.updated_at
                 WHERE messages.payload_hash != excluded.payload_hash",
            )?;
            let mut upsert_list = transaction.prepare(
                "INSERT INTO message_list(account_key, folder_key, uid, payload)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload = excluded.payload",
            )?;
            for message in messages {
                let (payload, digest, list_payload) = encrypt_message_payloads(message)?;
                upsert_message.execute(params![
                    account_key,
                    folder_key,
                    i64::from(message.uid),
                    digest,
                    payload,
                    now
                ])?;
                upsert_list.execute(params![
                    account_key,
                    folder_key,
                    i64::from(message.uid),
                    list_payload
                ])?;
            }
        }
        transaction.execute(
            "DELETE FROM messages
             WHERE account_key = ?1 AND folder_key = ?2 AND uid IN (
               SELECT uid FROM messages
               WHERE account_key = ?1 AND folder_key = ?2
               ORDER BY uid DESC LIMIT -1 OFFSET ?3
             )",
            params![account_key, folder_key, max_messages],
        )?;
        let (oldest, message_count, list_count): (Option<i64>, i64, i64) = transaction.query_row(
            "SELECT MIN(uid), COUNT(*), (
               SELECT COUNT(*) FROM message_list
               WHERE account_key = ?1 AND folder_key = ?2
             ) FROM messages
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        metadata.oldest_uid = oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        let (metadata_payload, _) = encrypt_json(&metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1,
               message_count = ?4, list_count = ?5, updated_at = ?6
             WHERE account_key = ?1 AND folder_key = ?2",
            params![
                account_key,
                folder_key,
                metadata_payload,
                message_count.max(0),
                list_count.max(0),
                now
            ],
        )?;
        transaction.commit()?;
        Ok(())
    })
}

/// Persist one synchronization delta without rewriting the rest of the mailbox. New headers are
/// inserted directly, removed UIDs cascade through list/search rows, and flag changes decrypt and
/// rewrite only their exact full-body row so cached MIME bodies remain intact. An empty delta is a
/// constant-size metadata update even when the mailbox contains thousands of messages.
#[allow(clippy::too_many_arguments)]
pub fn apply_mailbox_sync_delta(
    cache_root: &Path,
    account_id: &str,
    mailbox: &CachedMailbox,
    new_messages: &[CachedMessage],
    flag_updates: &[(u32, bool, bool)],
    removed_uids: &[u32],
    max_messages: usize,
) -> Result<PathBuf> {
    if mailbox.account_id != account_id {
        return Err(anyhow!("indexed mail cache account mismatch"));
    }
    if new_messages.iter().any(|message| {
        message.account_id != account_id || !message.folder.eq_ignore_ascii_case(&mailbox.folder)
    }) {
        return Err(anyhow!("indexed message cache identity mismatch"));
    }
    let mut new_by_uid = HashMap::new();
    for message in new_messages {
        if new_by_uid.insert(message.uid, message).is_some() {
            return Err(anyhow!("duplicate message UID in synchronization delta"));
        }
    }
    let mut coalesced_flags = HashMap::new();
    for (uid, unread, starred) in flag_updates {
        if *uid == 0 {
            return Err(anyhow!("invalid message UID in synchronization delta"));
        }
        coalesced_flags.insert(*uid, (*unread, *starred));
    }
    let mut removed = removed_uids.iter().copied().collect::<HashSet<_>>();
    removed.retain(|uid| !new_by_uid.contains_key(uid));
    if removed.contains(&0) {
        return Err(anyhow!("invalid message UID in synchronization delta"));
    }
    let max_messages = i64::try_from(max_messages.max(1)).unwrap_or(i64::MAX);

    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, &mailbox.folder);
        let now = now_epoch_millis();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read_metadata_for_page(&transaction, &account_key, &folder_key)?;
        let (mut message_count, mut list_count) =
            if let Some((metadata, _, count, _complete)) = existing.as_ref() {
                validate_identity(metadata, account_id, &mailbox.folder)?;
                let list_count: i64 = transaction.query_row(
                    "SELECT list_count FROM mailbox_meta
                 WHERE account_key = ?1 AND folder_key = ?2",
                    params![account_key, folder_key],
                    |row| row.get(0),
                )?;
                (i64::try_from(*count).unwrap_or(i64::MAX), list_count.max(0))
            } else {
                let delta_uids = new_by_uid.keys().copied().collect::<HashSet<_>>();
                if mailbox
                    .messages
                    .iter()
                    .any(|message| !delta_uids.contains(&message.uid))
                {
                    return Err(anyhow!(
                        "cannot create indexed mailbox metadata from an incomplete delta"
                    ));
                }
                (0, 0)
            };
        if existing.is_none() {
            let initial_payload = encrypt_json(&MailboxMetadata::from(mailbox))?.0;
            transaction.execute(
                "INSERT INTO mailbox_meta(account_key, folder_key, payload, revision, message_count, list_count, updated_at)
                 VALUES (?1, ?2, ?3, 0, 0, 0, ?4)",
                params![account_key, folder_key, initial_payload, now],
            )?;
        }

        if new_by_uid.is_empty()
            && coalesced_flags.is_empty()
            && removed.is_empty()
            && message_count <= max_messages
        {
            let metadata_payload = encrypt_json(&MailboxMetadata::from(mailbox))?.0;
            transaction.execute(
                "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1,
                   updated_at = ?4
                 WHERE account_key = ?1 AND folder_key = ?2",
                params![account_key, folder_key, metadata_payload, now],
            )?;
            transaction.commit()?;
            return Ok(());
        }

        {
            let mut existence = transaction.prepare(
                "SELECT
                   EXISTS(SELECT 1 FROM messages
                          WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3),
                   EXISTS(SELECT 1 FROM message_list
                          WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3)",
            )?;
            let mut delete = transaction.prepare(
                "DELETE FROM messages
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
            )?;
            for uid in &removed {
                let (message_existed, list_existed): (bool, bool) = existence
                    .query_row(params![account_key, folder_key, i64::from(*uid)], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?;
                if message_existed {
                    delete.execute(params![account_key, folder_key, i64::from(*uid)])?;
                    message_count = message_count.saturating_sub(1);
                    if list_existed {
                        list_count = list_count.saturating_sub(1);
                    }
                }
            }
        }

        for (uid, (unread, starred)) in &coalesced_flags {
            if removed.contains(uid) || new_by_uid.contains_key(uid) {
                continue;
            }
            let payload = transaction
                .query_row(
                    "SELECT payload FROM messages
                     WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
                    params![account_key, folder_key, i64::from(*uid)],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            let Some(payload) = payload else { continue };
            let mut message = decrypt_message(&payload, account_id, &mailbox.folder, *uid)?;
            if message.unread == *unread && message.starred == *starred {
                continue;
            }
            message.unread = *unread;
            message.starred = *starred;
            let (payload, digest, list_payload) = encrypt_message_payloads(&message)?;
            transaction.execute(
                "UPDATE messages SET payload_hash = ?4, payload = ?5, updated_at = ?6
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
                params![
                    account_key,
                    folder_key,
                    i64::from(*uid),
                    digest,
                    payload,
                    now
                ],
            )?;
            let list_existed: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM message_list
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3)",
                params![account_key, folder_key, i64::from(*uid)],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT INTO message_list(account_key, folder_key, uid, payload)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload = excluded.payload",
                params![account_key, folder_key, i64::from(*uid), list_payload],
            )?;
            if !list_existed {
                list_count = list_count.saturating_add(1);
            }
        }

        {
            let mut existence = transaction.prepare(
                "SELECT
                   EXISTS(SELECT 1 FROM messages
                          WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3),
                   EXISTS(SELECT 1 FROM message_list
                          WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3)",
            )?;
            let mut upsert_message = transaction.prepare(
                "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, recipient_version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload_hash = excluded.payload_hash,
                   payload = excluded.payload,
                   search_version = 0,
                   recipient_version = 0,
                   updated_at = excluded.updated_at
                 WHERE messages.payload_hash != excluded.payload_hash",
            )?;
            let mut upsert_list = transaction.prepare(
                "INSERT INTO message_list(account_key, folder_key, uid, payload)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload = excluded.payload",
            )?;
            for message in new_by_uid.values() {
                let uid = i64::from(message.uid);
                let (message_existed, list_existed): (bool, bool) = existence
                    .query_row(params![account_key, folder_key, uid], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?;
                let (payload, digest, list_payload) = encrypt_message_payloads(message)?;
                upsert_message.execute(params![
                    account_key,
                    folder_key,
                    uid,
                    digest,
                    payload,
                    now
                ])?;
                upsert_list.execute(params![account_key, folder_key, uid, list_payload])?;
                if !message_existed {
                    message_count = message_count.saturating_add(1);
                }
                if !list_existed {
                    list_count = list_count.saturating_add(1);
                }
            }
        }

        let overflow = {
            let mut statement = transaction.prepare(
                "SELECT messages.uid,
                   EXISTS(SELECT 1 FROM message_list
                          WHERE message_list.account_key = messages.account_key
                            AND message_list.folder_key = messages.folder_key
                            AND message_list.uid = messages.uid)
                 FROM messages
                 WHERE account_key = ?1 AND folder_key = ?2
                 ORDER BY uid DESC LIMIT -1 OFFSET ?3",
            )?;
            let rows = statement
                .query_map(params![account_key, folder_key, max_messages], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
                })?;
            let mut values = Vec::new();
            for row in rows {
                values.push(row?);
            }
            values
        };
        if !overflow.is_empty() {
            let mut delete = transaction.prepare(
                "DELETE FROM messages
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
            )?;
            for (uid, list_existed) in &overflow {
                delete.execute(params![account_key, folder_key, uid])?;
                message_count = message_count.saturating_sub(1);
                if *list_existed {
                    list_count = list_count.saturating_sub(1);
                }
            }
        }
        let oldest: Option<i64> = transaction.query_row(
            "SELECT MIN(uid) FROM messages
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| row.get(0),
        )?;
        let mut metadata = MailboxMetadata::from(mailbox);
        metadata.oldest_uid = oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        metadata.has_more |= !overflow.is_empty();
        let metadata_payload = encrypt_json(&metadata)?.0;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1,
               message_count = ?4, list_count = ?5, updated_at = ?6
             WHERE account_key = ?1 AND folder_key = ?2",
            params![
                account_key,
                folder_key,
                metadata_payload,
                message_count,
                list_count,
                now
            ],
        )?;
        transaction.commit()?;
        Ok(())
    })?;
    Ok(database_path(cache_root))
}

pub fn move_message(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    target_folder: &str,
    synced_at: &str,
    max_messages: usize,
) -> Result<bool> {
    if folder.eq_ignore_ascii_case(target_folder) {
        return Ok(false);
    }
    let max_messages = i64::try_from(max_messages.max(1)).unwrap_or(i64::MAX);
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let source_folder_key = folder_identity_key(account_id, folder);
        let target_folder_key = folder_identity_key(account_id, target_folder);
        let now = now_epoch_millis();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((mut source_metadata, _)) =
            read_metadata(&transaction, &account_key, &source_folder_key)?
        else {
            return Ok(false);
        };
        validate_identity(&source_metadata, account_id, folder)?;
        let source_payload = transaction
            .query_row(
                "SELECT payload FROM messages
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
                params![account_key, source_folder_key, i64::from(uid)],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let Some(source_payload) = source_payload else {
            transaction.commit()?;
            return Ok(false);
        };
        let mut message = decrypt_message(&source_payload, account_id, folder, uid)?;
        let target_existing = read_metadata(&transaction, &account_key, &target_folder_key)?;
        let mut target_metadata = target_existing
            .as_ref()
            .map(|(metadata, _)| metadata.clone())
            .unwrap_or_else(|| MailboxMetadata {
                schema_version: CACHE_SCHEMA_VERSION,
                account_id: account_id.to_string(),
                folder: target_folder.to_string(),
                uid_validity: None,
                highest_mod_seq: None,
                synced_at: synced_at.to_string(),
                oldest_uid: None,
                has_more: false,
            });
        validate_identity(&target_metadata, account_id, target_folder)?;
        if target_existing.is_none() {
            let (target_payload, _) = encrypt_json(&target_metadata)?;
            transaction.execute(
                "INSERT INTO mailbox_meta(account_key, folder_key, payload, revision, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![account_key, target_folder_key, target_payload, now],
            )?;
        }

        message.folder = target_folder.to_string();
        let (message_payload, message_digest, list_payload) = encrypt_message_payloads(&message)?;
        transaction.execute(
            "DELETE FROM messages
             WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
            params![account_key, source_folder_key, i64::from(uid)],
        )?;
        transaction.execute(
            "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, recipient_version, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)
             ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
               payload_hash = excluded.payload_hash,
               payload = excluded.payload,
               search_version = 0,
               recipient_version = 0,
               updated_at = excluded.updated_at",
            params![
                account_key,
                target_folder_key,
                i64::from(uid),
                message_digest,
                message_payload,
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO message_list(account_key, folder_key, uid, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
               payload = excluded.payload",
            params![account_key, target_folder_key, i64::from(uid), list_payload],
        )?;
        transaction.execute(
            "DELETE FROM messages
             WHERE account_key = ?1 AND folder_key = ?2 AND uid IN (
               SELECT uid FROM messages
               WHERE account_key = ?1 AND folder_key = ?2
               ORDER BY uid DESC LIMIT -1 OFFSET ?3
             )",
            params![account_key, target_folder_key, max_messages],
        )?;

        let (source_oldest, source_count, source_list_count): (Option<i64>, i64, i64) = transaction
            .query_row(
                "SELECT MIN(uid), COUNT(*), (
               SELECT COUNT(*) FROM message_list
               WHERE account_key = ?1 AND folder_key = ?2
             ) FROM messages
             WHERE account_key = ?1 AND folder_key = ?2",
                params![account_key, source_folder_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        source_metadata.oldest_uid = source_oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        source_metadata.synced_at = synced_at.to_string();
        let (source_metadata_payload, _) = encrypt_json(&source_metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1,
               message_count = ?4, list_count = ?5, updated_at = ?6
             WHERE account_key = ?1 AND folder_key = ?2",
            params![
                account_key,
                source_folder_key,
                source_metadata_payload,
                source_count.max(0),
                source_list_count.max(0),
                now
            ],
        )?;

        let (target_oldest, target_count, target_list_count): (Option<i64>, i64, i64) = transaction
            .query_row(
                "SELECT MIN(uid), COUNT(*), (
               SELECT COUNT(*) FROM message_list
               WHERE account_key = ?1 AND folder_key = ?2
             ) FROM messages
             WHERE account_key = ?1 AND folder_key = ?2",
                params![account_key, target_folder_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        target_metadata.oldest_uid = target_oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        target_metadata.synced_at = synced_at.to_string();
        let (target_metadata_payload, _) = encrypt_json(&target_metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1,
               message_count = ?4, list_count = ?5, updated_at = ?6
             WHERE account_key = ?1 AND folder_key = ?2",
            params![
                account_key,
                target_folder_key,
                target_metadata_payload,
                target_count.max(0),
                target_list_count.max(0),
                now
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    })
}

pub fn remove_message(cache_root: &Path, account_id: &str, folder: &str, uid: u32) -> Result<()> {
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((mut metadata, _)) = read_metadata(&transaction, &account_key, &folder_key)?
        else {
            return Ok(());
        };
        validate_identity(&metadata, account_id, folder)?;
        let removed = transaction.execute(
            "DELETE FROM messages
             WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
            params![account_key, folder_key, i64::from(uid)],
        )?;
        if removed == 0 {
            transaction.commit()?;
            return Ok(());
        }
        let (oldest, message_count, list_count): (Option<i64>, i64, i64) = transaction.query_row(
            "SELECT MIN(uid), COUNT(*), (
               SELECT COUNT(*) FROM message_list
               WHERE account_key = ?1 AND folder_key = ?2
             ) FROM messages
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        metadata.oldest_uid = oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        metadata.synced_at = format!("unix:{}", now_epoch_millis() / 1000);
        let (metadata_payload, _) = encrypt_json(&metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1,
               message_count = ?4, list_count = ?5, updated_at = ?6
             WHERE account_key = ?1 AND folder_key = ?2",
            params![
                account_key,
                folder_key,
                metadata_payload,
                message_count.max(0),
                list_count.max(0),
                now_epoch_millis()
            ],
        )?;
        transaction.commit()?;
        Ok(())
    })
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<()> {
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        connection.execute(
            "DELETE FROM mailbox_meta WHERE account_key = ?1",
            params![account_key],
        )?;
        // A successful account removal must not leave a recovery copy that can resurrect the
        // removed account's protected mail rows after a later database failure.
        refresh_backup_with_connection(connection, cache_root, true)
            .map_err(backup_maintenance_error)
            .context("refresh indexed cache recovery copy after account removal")?;
        remove_file_if_exists(&cache_root.join(DATABASE_BACKUP_PREVIOUS_FILE))?;
        remove_file_if_exists(&cache_root.join(DATABASE_BACKUP_PENDING_FILE))
    })
}

#[cfg(test)]
pub fn encrypted_payload_for_test(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<Option<Vec<u8>>> {
    with_recovery(cache_root, |connection| {
        connection
            .query_row(
                "SELECT payload FROM messages
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
                params![
                    identity_key(account_id),
                    folder_identity_key(account_id, folder),
                    i64::from(uid)
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::SmartCategory;

    fn temporary_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mailgo-cache-db-{label}-{}-{}",
            std::process::id(),
            now_epoch_millis()
        ))
    }

    fn fixture_message(uid: u32) -> CachedMessage {
        CachedMessage {
            id: format!("fixture:{uid}"),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_id: format!("thread:{uid}"),
            account_id: "fixture-account".into(),
            folder: "INBOX".into(),
            uid,
            subject: format!("subject {uid}"),
            sender_name: "Sender".into(),
            sender_email: "sender@example.invalid".into(),
            to: Vec::new(),
            cc: Vec::new(),
            received_at: None,
            unread: true,
            starred: false,
            category: SmartCategory::Inbox,
            is_ad: false,
            preview: "preview".into(),
            text_body: String::new(),
            html_body: None,
            attachments: Vec::new(),
            raw_path: None,
        }
    }

    fn insert_corrupt_rows(root: &Path, folder: &str, first_uid: u32, last_uid: u32) {
        let mut connection = open(root).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .unwrap();
            for uid in first_uid..=last_uid {
                insert
                    .execute(params![
                        identity_key("fixture-account"),
                        folder_identity_key("fixture-account", folder),
                        i64::from(uid),
                        [0u8; 32],
                        vec![0u8; 16],
                        now_epoch_millis()
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
    }

    fn message_count(root: &Path, folder: &str) -> usize {
        let connection = open(root).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE account_key = ?1 AND folder_key = ?2",
                params![
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", folder)
                ],
                |row| row.get(0),
            )
            .unwrap();
        count as usize
    }

    fn index_every_message(root: &Path) {
        loop {
            let progress = rebuild_search_index_batch(root, SEARCH_INDEX_BATCH_SIZE).unwrap();
            if !progress.has_more {
                break;
            }
            assert!(progress.indexed > 0);
        }
    }

    fn stored_search_terms(root: &Path) -> Vec<Vec<u8>> {
        let connection = open(root).unwrap();
        let mut statement = connection
            .prepare("SELECT term FROM message_search_terms ORDER BY term")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    fn stored_recipient_terms(root: &Path) -> Vec<Vec<u8>> {
        let connection = open(root).unwrap();
        let mut statement = connection
            .prepare("SELECT term FROM recipient_search_terms ORDER BY term")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn encryption_migration_queries_use_small_version_indexes() {
        let root = temporary_root("encryption-plan");
        let connection = open(&root).unwrap();
        for (table, ordering, expected_index) in [
            (
                "mailbox_meta",
                "account_key, folder_key",
                "mailbox_meta_encryption_migration",
            ),
            (
                "message_list",
                "account_key, folder_key, uid",
                "message_list_encryption_migration",
            ),
            (
                "messages",
                "account_key, folder_key, uid",
                "messages_encryption_migration",
            ),
        ] {
            let detail: String = connection
                .query_row(
                    &format!(
                        "EXPLAIN QUERY PLAN SELECT payload FROM {table}
                         WHERE encryption_version = 0 ORDER BY {ordering} LIMIT 32"
                    ),
                    [],
                    |row| row.get(3),
                )
                .unwrap();
            assert!(detail.contains(expected_index), "unexpected plan: {detail}");
        }
        drop(connection);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn background_encryption_migration_rewrites_legacy_rows_without_changing_mail() {
        fn stored_payloads(root: &Path) -> Vec<Vec<u8>> {
            let connection = open(root).unwrap();
            let mut statement = connection
                .prepare(
                    "SELECT payload FROM mailbox_meta
                     UNION ALL SELECT payload FROM message_list
                     UNION ALL SELECT payload FROM messages",
                )
                .unwrap();
            let rows = statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .unwrap();
            rows.map(Result::unwrap).collect()
        }

        fn current_version_count(root: &Path) -> i64 {
            let connection = open(root).unwrap();
            connection
                .query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM mailbox_meta WHERE encryption_version = 1) +
                       (SELECT COUNT(*) FROM message_list WHERE encryption_version = 1) +
                       (SELECT COUNT(*) FROM messages WHERE encryption_version = 1)",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        }

        let root = temporary_root("encryption-migration");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(1), fixture_message(2)];
        mailbox.oldest_uid = Some(1);
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        assert_eq!(current_version_count(&root), 5);

        {
            let connection = open(&root).unwrap();
            for table in ["mailbox_meta", "message_list", "messages"] {
                let rows = {
                    let mut statement = connection
                        .prepare(&format!("SELECT payload FROM {table}"))
                        .unwrap();
                    let values = statement
                        .query_map([], |row| row.get::<_, Vec<u8>>(0))
                        .unwrap();
                    values.map(Result::unwrap).collect::<Vec<_>>()
                };
                for payload in rows {
                    let plaintext = crate::sync::unprotect_database_cache(&payload).unwrap();
                    let legacy = crate::sync::protect_cache(&plaintext).unwrap();
                    connection
                        .execute(
                            &format!("UPDATE {table} SET payload = ?1 WHERE payload = ?2"),
                            params![legacy, payload],
                        )
                        .unwrap();
                }
            }
        }
        let legacy_payloads = stored_payloads(&root);
        assert_eq!(legacy_payloads.len(), 5);
        assert!(legacy_payloads
            .iter()
            .all(|payload| !crate::sync::database_cache_uses_current_envelope(payload)));
        assert_eq!(current_version_count(&root), 0);

        let mut migrated = 0usize;
        for table in [
            EncryptedPayloadTable::MessageList,
            EncryptedPayloadTable::MailboxMetadata,
            EncryptedPayloadTable::Messages,
        ] {
            let (table_migrated, skipped) = migrate_encrypted_payload_table(&root, table).unwrap();
            assert_eq!(skipped, 0);
            migrated += table_migrated;
        }
        assert_eq!(migrated, 5);
        assert!(stored_payloads(&root)
            .iter()
            .all(|payload| crate::sync::database_cache_uses_current_envelope(payload)));
        assert_eq!(current_version_count(&root), 5);

        let page = load_mailbox_page(&root, "fixture-account", "INBOX", None, 2)
            .unwrap()
            .unwrap();
        assert_eq!(page.mailbox.messages.len(), 2);
        assert_eq!(
            load_message(&root, "fixture-account", "INBOX", 1)
                .unwrap()
                .unwrap()
                .subject,
            "subject 1"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn migrates_schema_one_to_the_blind_search_schema() {
        let root = temporary_root("search-migration");
        fs::create_dir_all(&root).unwrap();
        let connection = Connection::open(database_path(&root)).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE mailbox_meta (
                    account_key BLOB NOT NULL,
                    folder_key BLOB NOT NULL,
                    payload BLOB NOT NULL,
                    revision INTEGER NOT NULL DEFAULT 1,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (account_key, folder_key)
                 ) WITHOUT ROWID;
                 CREATE TABLE messages (
                    account_key BLOB NOT NULL,
                    folder_key BLOB NOT NULL,
                    uid INTEGER NOT NULL,
                    payload_hash BLOB NOT NULL,
                    payload BLOB NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (account_key, folder_key, uid),
                    FOREIGN KEY (account_key, folder_key)
                        REFERENCES mailbox_meta(account_key, folder_key) ON DELETE CASCADE
                 ) WITHOUT ROWID;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        let mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        let (metadata_payload, _) = encrypt_json(&MailboxMetadata::from(&mailbox)).unwrap();
        let message = fixture_message(1);
        let (message_payload, digest) = encrypt_json(&message).unwrap();
        connection
            .execute(
                "INSERT INTO mailbox_meta(account_key, folder_key, payload, revision, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", "INBOX"),
                    metadata_payload,
                    now_epoch_millis()
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, updated_at)
                 VALUES (?1, ?2, 1, ?3, ?4, ?5)",
                params![
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", "INBOX"),
                    digest,
                    message_payload,
                    now_epoch_millis()
                ],
            )
            .unwrap();
        drop(connection);

        let connection = open(&root).unwrap();
        let schema_version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let search_column: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'search_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let recipient_column: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'recipient_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let list_table: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'message_list'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let count_column: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('mailbox_meta') WHERE name = 'message_count'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let list_count_column: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('mailbox_meta') WHERE name = 'list_count'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (migrated_count, migrated_list_count): (i64, i64) = connection
            .query_row(
                "SELECT message_count, list_count FROM mailbox_meta",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let legacy_summaries: i64 = connection
            .query_row("SELECT COUNT(*) FROM message_list", [], |row| row.get(0))
            .unwrap();
        let search_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('message_search_terms', 'search_index_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let recipient_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'recipient_search_terms'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_version, DATABASE_SCHEMA_VERSION);
        assert_eq!(search_column, 1);
        assert_eq!(recipient_column, 1);
        assert_eq!(list_table, 1);
        assert_eq!(count_column, 1);
        assert_eq!(list_count_column, 1);
        assert_eq!(migrated_count, 1);
        assert_eq!(migrated_list_count, 0);
        assert_eq!(legacy_summaries, 0);
        assert_eq!(search_tables, 2);
        assert_eq!(recipient_tables, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blind_terms_are_bounded_deterministic_and_keyed() {
        let mut message = fixture_message(1);
        let varied = (0..2_000)
            .filter_map(|offset| char::from_u32(0x4e00 + offset))
            .collect::<String>();
        message.subject = varied.clone();
        let first_key = [7u8; SEARCH_KEY_BYTES];
        let second_key = [9u8; SEARCH_KEY_BYTES];
        let first = message_search_terms(&first_key, &message).unwrap();
        let repeated = message_search_terms(&first_key, &message).unwrap();
        let rotated = message_search_terms(&second_key, &message).unwrap();
        let query = query_search_terms(&first_key, &query_search_words(&varied)).unwrap();
        assert_eq!(first, repeated);
        assert_ne!(first, rotated);
        assert_eq!(first.len(), MAX_SEARCH_TERMS_PER_MESSAGE);
        assert_eq!(query.len(), MAX_SEARCH_QUERY_TERMS);
    }

    #[test]
    fn full_cache_search_finds_rows_outside_the_first_renderer_page() {
        let root = temporary_root("full-cache-search");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = (1..=80).map(fixture_message).collect();
        mailbox.messages[0].subject = "archivedneedletoken planning".into();
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let first_page = load_mailbox_page(&root, "fixture-account", "INBOX", None, 10)
            .unwrap()
            .unwrap();
        assert!(!first_page
            .mailbox
            .messages
            .iter()
            .any(|message| message.uid == 1));
        index_every_message(&root);

        let result = search_messages(
            &root,
            &["fixture-account".into()],
            "archivedneedletoken",
            10,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].uid, 1);
        assert!(!result.indexing);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_search_is_account_scoped_and_can_match_cached_body_text() {
        let root = temporary_root("search-scope-body");
        let mut first = fixture_message(1);
        first.text_body = "bodyonlyneedletoken".into();
        let mut first_mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        first_mailbox.messages = vec![first];
        save_mailbox(&root, "fixture-account", &first_mailbox).unwrap();

        let mut second = fixture_message(2);
        second.id = "second:2".into();
        second.account_id = "second-account".into();
        second.text_body = "bodyonlyneedletoken".into();
        let mut second_mailbox = CachedMailbox::empty("second-account", "INBOX");
        second_mailbox.messages = vec![second];
        save_mailbox(&root, "second-account", &second_mailbox).unwrap();
        index_every_message(&root);

        let result =
            search_messages(&root, &["second-account".into()], "bodyonlyneedletoken", 10).unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].account_id, "second-account");
        assert_eq!(result.messages[0].uid, 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recipient_suggestions_are_account_scoped_ranked_and_deduplicated() {
        let root = temporary_root("recipient-suggestions");
        let mut first = fixture_message(1);
        first.sender_name = "Alice Example".into();
        first.sender_email = "alice@example.invalid".into();
        first.to = vec!["owner@example.invalid".into(), "bob@example.invalid".into()];
        first.received_at = Some("2026-01-01T08:00:00Z".into());
        let mut second = fixture_message(2);
        second.sender_name = "Alice Example".into();
        second.sender_email = "ALICE@example.invalid".into();
        second.received_at = Some("2026-01-02T08:00:00Z".into());
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![first, second];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();

        let mut other = fixture_message(3);
        other.account_id = "other-account".into();
        other.id = "other:3".into();
        other.sender_name = "Alice Other".into();
        other.sender_email = "alice-other@example.invalid".into();
        let mut other_mailbox = CachedMailbox::empty("other-account", "INBOX");
        other_mailbox.messages = vec![other];
        save_mailbox(&root, "other-account", &other_mailbox).unwrap();
        index_every_message(&root);

        let result = suggest_recipients(
            &root,
            "fixture-account",
            "owner@example.invalid",
            "alice",
            8,
        )
        .unwrap();
        assert_eq!(result.suggestions.len(), 1);
        assert_eq!(result.suggestions[0].email, "ALICE@example.invalid");
        assert_eq!(result.suggestions[0].name, "Alice Example");
        assert_eq!(result.suggestions[0].frequency, 2);
        assert_eq!(
            result.suggestions[0].last_seen.as_deref(),
            Some("2026-01-02T08:00:00Z")
        );
        assert!(!result.indexing);

        let owner = suggest_recipients(
            &root,
            "fixture-account",
            "owner@example.invalid",
            "owner",
            8,
        )
        .unwrap();
        assert!(owner.suggestions.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recipient_blind_index_finds_old_contacts_from_body_free_rows() {
        let root = temporary_root("recipient-old-indexed");
        let mut messages = (1..=600).map(fixture_message).collect::<Vec<_>>();
        messages[0].sender_name = "Unique Old Contact".into();
        messages[0].sender_email = "unique-old-contact@example.invalid".into();
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = messages;
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        index_every_message(&root);

        let connection = open(&root).unwrap();
        connection
            .execute(
                "UPDATE messages SET payload = X'00'
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = 1",
                params![
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", "INBOX")
                ],
            )
            .unwrap();
        drop(connection);

        let result = suggest_recipients(
            &root,
            "fixture-account",
            "owner@example.invalid",
            "unique-old",
            8,
        )
        .unwrap();
        assert_eq!(result.suggestions.len(), 1);
        assert_eq!(
            result.suggestions[0].email,
            "unique-old-contact@example.invalid"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recipient_index_excludes_subject_preview_and_body_terms() {
        let root = temporary_root("recipient-field-only");
        let mut message = fixture_message(1);
        message.sender_name = "Alice".into();
        message.sender_email = "alice@example.invalid".into();
        message.subject = "bodyonlyrecipientneedle".into();
        message.preview = "bodyonlyrecipientneedle".into();
        message.text_body = "bodyonlyrecipientneedle".into();
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![message];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        index_every_message(&root);

        let result = suggest_recipients(
            &root,
            "fixture-account",
            "owner@example.invalid",
            "bodyonlyrecipientneedle",
            8,
        )
        .unwrap();
        assert!(result.suggestions.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn blind_index_and_protected_key_never_store_search_plaintext() {
        let root = temporary_root("search-privacy");
        let plaintext = "mailgoprivacycanarytoken";
        let mut message = fixture_message(5);
        message.subject = plaintext.into();
        message.sender_email = format!("{plaintext}@example.invalid");
        message.text_body = format!("cached body {plaintext}");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![message];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        index_every_message(&root);
        let connection = open(&root).unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(connection);

        for entry in fs::read_dir(&root).unwrap().flatten() {
            if !entry.file_type().unwrap().is_file() {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(plaintext));
        }
        let result = search_messages(&root, &["fixture-account".into()], plaintext, 10).unwrap();
        assert_eq!(result.messages.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn one_corrupt_message_does_not_stall_search_index_rebuild() {
        let root = temporary_root("search-corrupt-row");
        let mut healthy = fixture_message(500);
        healthy.subject = "healthysearchneedle".into();
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![healthy];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        insert_corrupt_rows(&root, "INBOX", 1, 1);
        index_every_message(&root);

        let connection = open(&root).unwrap();
        let incomplete: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE search_version != ?1",
                params![SEARCH_INDEX_VERSION],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(incomplete, 0);
        drop(connection);
        let result = search_messages(
            &root,
            &["fixture-account".into()],
            "healthysearchneedle",
            10,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].uid, 500);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rotating_the_protected_search_key_rebuilds_every_blind_term() {
        let root = temporary_root("search-key-rotation");
        let mut message = fixture_message(7);
        message.subject = "rotationsearchneedle".into();
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![message];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        index_every_message(&root);
        let original_terms = stored_search_terms(&root);
        let original_recipient_terms = stored_recipient_terms(&root);
        assert!(!original_terms.is_empty());
        assert!(!original_recipient_terms.is_empty());

        let key_path = search_key_path(&root);
        fs::remove_file(&key_path).unwrap();
        write_protected_search_key(&key_path, &[29u8; SEARCH_KEY_BYTES]).unwrap();
        SEARCH_KEYS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(&key_path);
        index_every_message(&root);
        let rotated_terms = stored_search_terms(&root);
        let rotated_recipient_terms = stored_recipient_terms(&root);
        assert_ne!(original_terms, rotated_terms);
        assert_ne!(original_recipient_terms, rotated_recipient_terms);
        let result = search_messages(
            &root,
            &["fixture-account".into()],
            "rotationsearchneedle",
            10,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unreadable_search_key_is_quarantined_and_rebuilt_without_losing_mail() {
        let root = temporary_root("invalid-search-key");
        let mut message = fixture_message(8);
        message.subject = "recoveredsearchneedle".into();
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![message];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        index_every_message(&root);

        let key_path = search_key_path(&root);
        fs::write(&key_path, b"not a protected search key").unwrap();
        SEARCH_KEYS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .remove(&key_path);
        index_every_message(&root);

        assert!(fs::read_dir(&root).unwrap().flatten().any(|entry| entry
            .file_name()
            .to_string_lossy()
            .starts_with("search-index-key-v1.bin.invalid-")));
        let result = search_messages(
            &root,
            &["fixture-account".into()],
            "recoveredsearchneedle",
            10,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(
            load_message(&root, "fixture-account", "INBOX", 8)
                .unwrap()
                .unwrap()
                .uid,
            8
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pages_in_uid_order_without_decrypting_the_whole_mailbox() {
        let root = temporary_root("paging");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = (1..=240).rev().map(fixture_message).collect();
        mailbox.oldest_uid = Some(1);
        mailbox.has_more = true;
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();

        let first = load_mailbox_page(&root, "fixture-account", "INBOX", None, 50)
            .unwrap()
            .unwrap();
        assert_eq!(first.mailbox.messages.len(), 50);
        assert_eq!(first.mailbox.messages.first().unwrap().uid, 240);
        assert_eq!(first.mailbox.messages.last().unwrap().uid, 191);
        assert!(first.local_has_more);
        assert_eq!(first.total_cached, 240);

        let second = load_mailbox_page(&root, "fixture-account", "INBOX", Some(191), 50)
            .unwrap()
            .unwrap();
        assert_eq!(second.mailbox.messages.first().unwrap().uid, 190);
        assert_eq!(second.mailbox.messages.last().unwrap().uid, 141);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_pages_use_small_summaries_while_exact_reads_keep_the_full_body() {
        let root = temporary_root("list-summary");
        let mut message = fixture_message(7);
        message.preview = "bounded preview".into();
        message.text_body = "x".repeat(2 * 1024 * 1024);
        message.html_body = Some(format!("<p>{}</p>", "y".repeat(512 * 1024)));
        message.attachments.push(crate::mail::CachedAttachment {
            index: 0,
            file_name: "report.pdf".into(),
            content_type: "application/pdf".into(),
            content_id: None,
            size: 128,
            cache_path: Some("private-cache-path".into()),
        });
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![message];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();

        let connection = open(&root).unwrap();
        let (payload_bytes, list_bytes, message_count, list_payload): (i64, i64, i64, Vec<u8>) =
            connection
                .query_row(
                    "SELECT LENGTH(messages.payload), LENGTH(message_list.payload),
                        mailbox_meta.message_count, message_list.payload
                 FROM messages
                 JOIN message_list USING(account_key, folder_key, uid)
                 JOIN mailbox_meta USING(account_key, folder_key)
                 WHERE messages.account_key = ?1 AND messages.folder_key = ?2 AND uid = 7",
                    params![
                        identity_key("fixture-account"),
                        folder_identity_key("fixture-account", "INBOX")
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
        drop(connection);
        assert_eq!(message_count, 1);
        assert!(list_bytes * 100 < payload_bytes);
        assert!(!list_payload
            .windows(b"bounded preview".len())
            .any(|window| window == b"bounded preview"));
        assert!(!list_payload
            .windows(b"report.pdf".len())
            .any(|window| window == b"report.pdf"));

        let page = load_mailbox_page(&root, "fixture-account", "INBOX", None, 20)
            .unwrap()
            .unwrap();
        let summary = &page.mailbox.messages[0];
        assert!(summary.text_body.is_empty());
        assert!(summary.html_body.is_none());
        assert_eq!(summary.preview, "bounded preview");
        assert_eq!(summary.attachments.len(), 1);
        assert!(summary.attachments[0].cache_path.is_none());

        let exact = load_message(&root, "fixture-account", "INBOX", 7)
            .unwrap()
            .unwrap();
        assert_eq!(exact.text_body.len(), 2 * 1024 * 1024);
        assert!(exact.html_body.is_some());
        assert_eq!(
            exact.attachments[0].cache_path.as_deref(),
            Some("private-cache-path")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_rows_gain_list_summaries_in_bounded_background_batches() {
        let root = temporary_root("list-backfill");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = (1..=90).rev().map(fixture_message).collect();
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let connection = open(&root).unwrap();
        connection.execute("DELETE FROM message_list", []).unwrap();
        connection
            .execute("UPDATE mailbox_meta SET list_count = 0", [])
            .unwrap();
        drop(connection);

        let first = rebuild_list_index_batch(&root, LIST_INDEX_BATCH_SIZE).unwrap();
        assert_eq!(first.indexed, LIST_INDEX_BATCH_SIZE);
        assert!(first.has_more);
        let second = rebuild_list_index_batch(&root, LIST_INDEX_BATCH_SIZE).unwrap();
        assert_eq!(second.indexed, 90 - LIST_INDEX_BATCH_SIZE);
        assert!(!second.has_more);

        let connection = open(&root).unwrap();
        let (summaries, list_count): (i64, i64) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM message_list), list_count FROM mailbox_meta",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(summaries, 90);
        assert_eq!(list_count, 90);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn paginated_query_uses_the_uid_range_from_the_primary_key() {
        let root = temporary_root("page-plan");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = (1..=10).rev().map(fixture_message).collect();
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let connection = open(&root).unwrap();
        let detail: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT uid, payload FROM message_list
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid < ?3
                 ORDER BY uid DESC LIMIT ?4",
                params![
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", "INBOX"),
                    9i64,
                    5i64
                ],
                |row| row.get(3),
            )
            .unwrap();
        let normalized = detail.to_ascii_lowercase().replace(' ', "");
        assert!(
            normalized.contains("uid<?"),
            "unexpected query plan: {detail}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn page_reads_do_not_touch_encrypted_rows_outside_the_requested_window() {
        let root = temporary_root("bounded-decryption");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = (1..=240).rev().map(fixture_message).collect();
        mailbox.oldest_uid = Some(1);
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let connection = open(&root).unwrap();
        connection
            .execute(
                "UPDATE messages SET payload = ?1
                 WHERE account_key = ?2 AND folder_key = ?3 AND uid = 1",
                params![
                    vec![0u8; 16],
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", "INBOX")
                ],
            )
            .unwrap();

        let page = load_mailbox_page(&root, "fixture-account", "INBOX", None, 50)
            .unwrap()
            .unwrap();
        assert_eq!(page.mailbox.messages.len(), 50);
        assert!(load_message(&root, "fixture-account", "INBOX", 1).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn synchronization_state_reads_only_body_free_summaries() {
        let root = temporary_root("sync-summaries");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = (1..=600).rev().map(fixture_message).collect();
        mailbox.messages[0].text_body = "large cached body".repeat(32_768);
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let connection = open(&root).unwrap();
        connection
            .execute(
                "UPDATE messages SET payload = ?1
                 WHERE account_key = ?2 AND folder_key = ?3",
                params![
                    vec![0u8; 16],
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", "INBOX")
                ],
            )
            .unwrap();
        drop(connection);

        let summaries =
            load_mailbox_summaries(&root, "fixture-account", "INBOX", MAX_SYNC_SUMMARIES)
                .unwrap()
                .unwrap();
        assert_eq!(summaries.messages.len(), 600);
        assert_eq!(summaries.messages.first().unwrap().uid, 600);
        assert!(summaries
            .messages
            .iter()
            .all(|message| message.text_body.is_empty() && message.html_body.is_none()));
        assert!(load_message(&root, "fixture-account", "INBOX", 600).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_sync_is_one_metadata_write_even_with_twenty_thousand_rows() {
        let root = temporary_root("metadata-only-sync");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(20_001)];
        mailbox.oldest_uid = Some(1);
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        insert_corrupt_rows(&root, "INBOX", 1, 20_000);
        let connection = open(&root).unwrap();
        connection
            .execute(
                "UPDATE mailbox_meta SET message_count = 20001, list_count = 1",
                [],
            )
            .unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_message_insert BEFORE INSERT ON messages
                   BEGIN SELECT RAISE(ABORT, 'unexpected message insert'); END;
                 CREATE TRIGGER reject_message_update BEFORE UPDATE ON messages
                   BEGIN SELECT RAISE(ABORT, 'unexpected message update'); END;
                 CREATE TRIGGER reject_message_delete BEFORE DELETE ON messages
                   BEGIN SELECT RAISE(ABORT, 'unexpected message delete'); END;
                 CREATE TRIGGER reject_list_insert BEFORE INSERT ON message_list
                   BEGIN SELECT RAISE(ABORT, 'unexpected list insert'); END;
                 CREATE TRIGGER reject_list_update BEFORE UPDATE ON message_list
                   BEGIN SELECT RAISE(ABORT, 'unexpected list update'); END;
                 CREATE TRIGGER reject_list_delete BEFORE DELETE ON message_list
                   BEGIN SELECT RAISE(ABORT, 'unexpected list delete'); END;",
            )
            .unwrap();
        drop(connection);

        mailbox.synced_at = "metadata-only-refresh".into();
        apply_mailbox_sync_delta(&root, "fixture-account", &mailbox, &[], &[], &[], 25_000)
            .unwrap();

        let connection = open(&root).unwrap();
        let (payload, stored_message_count, stored_list_count): (Vec<u8>, i64, i64) = connection
            .query_row(
                "SELECT payload, message_count, list_count FROM mailbox_meta",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let metadata: MailboxMetadata = decrypt_json(&payload, "mailbox metadata").unwrap();
        assert_eq!(metadata.synced_at, "metadata-only-refresh");
        assert_eq!(stored_message_count, 20_001);
        assert_eq!(stored_list_count, 1);
        assert_eq!(message_count(&root, "INBOX"), 20_001);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sync_delta_preserves_full_body_and_search_index_for_flag_changes() {
        let root = temporary_root("exact-sync-delta");
        let mut message = fixture_message(2);
        message.text_body = "x".repeat(2 * 1024 * 1024);
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![message, fixture_message(1)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let connection = open(&root).unwrap();
        connection
            .execute(
                "UPDATE messages SET search_version = 1, recipient_version = 1
                 WHERE account_key = ?1 AND folder_key = ?2 AND uid = 2",
                params![
                    identity_key("fixture-account"),
                    folder_identity_key("fixture-account", "INBOX")
                ],
            )
            .unwrap();
        drop(connection);

        let mut sync_state =
            load_mailbox_summaries(&root, "fixture-account", "INBOX", MAX_SYNC_SUMMARIES)
                .unwrap()
                .unwrap();
        sync_state.synced_at = "delta-refresh".into();
        sync_state.messages.retain(|message| message.uid != 1);
        sync_state.oldest_uid = Some(2);
        let changed = sync_state
            .messages
            .iter_mut()
            .find(|message| message.uid == 2)
            .unwrap();
        changed.unread = false;
        changed.starred = true;
        apply_mailbox_sync_delta(
            &root,
            "fixture-account",
            &sync_state,
            &[],
            &[(2, false, true)],
            &[1],
            MAX_SYNC_SUMMARIES,
        )
        .unwrap();

        let exact = load_message(&root, "fixture-account", "INBOX", 2)
            .unwrap()
            .unwrap();
        assert_eq!(exact.text_body.len(), 2 * 1024 * 1024);
        assert!(!exact.unread);
        assert!(exact.starred);
        assert!(load_message(&root, "fixture-account", "INBOX", 1)
            .unwrap()
            .is_none());
        let connection = open(&root).unwrap();
        let (message_count, list_count, search_version, recipient_version): (i64, i64, i64, i64) =
            connection
                .query_row(
                    "SELECT mailbox_meta.message_count, mailbox_meta.list_count,
                        messages.search_version, messages.recipient_version
                 FROM mailbox_meta JOIN messages USING(account_key, folder_key)
                 WHERE messages.uid = 2",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
        assert_eq!(
            (message_count, list_count, search_version, recipient_version),
            (1, 1, 1, 1)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exact_message_updates_do_not_replace_neighbor_rows() {
        let root = temporary_root("exact");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(2), fixture_message(1)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let neighbor_before = encrypted_payload_for_test(&root, "fixture-account", "INBOX", 1)
            .unwrap()
            .unwrap();
        let mut changed = fixture_message(2);
        changed.text_body = "downloaded body".into();
        save_message(&root, "fixture-account", &changed).unwrap();
        let neighbor_after = encrypted_payload_for_test(&root, "fixture-account", "INBOX", 1)
            .unwrap()
            .unwrap();
        assert_eq!(neighbor_before, neighbor_after);
        assert_eq!(
            load_message(&root, "fixture-account", "INBOX", 2)
                .unwrap()
                .unwrap()
                .text_body,
            "downloaded body"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mailbox_revision_tracks_page_changes_without_loading_messages() {
        let root = temporary_root("mailbox-revision");
        assert_eq!(
            mailbox_revision(&root, "fixture-account", "INBOX").unwrap(),
            None
        );
        save_mailbox(
            &root,
            "fixture-account",
            &CachedMailbox::empty("fixture-account", "INBOX"),
        )
        .unwrap();
        let initial = mailbox_revision(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap();

        save_message(&root, "fixture-account", &fixture_message(1)).unwrap();
        let changed = mailbox_revision(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap();
        assert!(changed > initial);
        let connection = Connection::open(database_path(&root)).unwrap();
        connection
            .execute("UPDATE messages SET payload = X'00'", [])
            .unwrap();
        drop(connection);
        assert_eq!(
            mailbox_revision(&root, "fixture-account", "INBOX").unwrap(),
            Some(changed),
            "revision polling must not touch encrypted message payloads"
        );
        assert_eq!(
            mailbox_revision(&root, "fixture-account", "Archive").unwrap(),
            None
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn encrypted_rows_do_not_contain_plaintext_subjects() {
        let root = temporary_root("encryption");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(7)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let payload = encrypted_payload_for_test(&root, "fixture-account", "INBOX", 7)
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&payload).contains("subject 7"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_row_transactions_preserve_every_message() {
        let root = temporary_root("concurrent");
        save_mailbox(
            &root,
            "fixture-account",
            &CachedMailbox::empty("fixture-account", "INBOX"),
        )
        .unwrap();
        let workers = (1..=12)
            .map(|uid| {
                let root = root.clone();
                std::thread::spawn(move || {
                    save_message(&root, "fixture-account", &fixture_message(uid)).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        let mailbox = load_mailbox(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap();
        assert_eq!(mailbox.messages.len(), 12);
        assert_eq!(mailbox.messages.first().unwrap().uid, 12);
        assert_eq!(mailbox.messages.last().unwrap().uid, 1);
        assert_eq!(
            load_mailbox_page(&root, "fixture-account", "INBOX", None, 5)
                .unwrap()
                .unwrap()
                .total_cached,
            12
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cached_message_counts_follow_insert_trim_move_and_remove_transactions() {
        let root = temporary_root("transactional-counts");
        save_mailbox(
            &root,
            "fixture-account",
            &CachedMailbox::empty("fixture-account", "INBOX"),
        )
        .unwrap();
        save_message(&root, "fixture-account", &fixture_message(1)).unwrap();
        save_message(&root, "fixture-account", &fixture_message(1)).unwrap();
        assert_eq!(
            load_mailbox_page(&root, "fixture-account", "INBOX", None, 10)
                .unwrap()
                .unwrap()
                .total_cached,
            1
        );

        merge_messages(
            &root,
            "fixture-account",
            "INBOX",
            None,
            "fixture-merge",
            &[fixture_message(2), fixture_message(3)],
            2,
        )
        .unwrap();
        assert_eq!(
            load_mailbox_page(&root, "fixture-account", "INBOX", None, 10)
                .unwrap()
                .unwrap()
                .total_cached,
            2
        );

        assert!(move_message(
            &root,
            "fixture-account",
            "INBOX",
            3,
            "Archive",
            "fixture-move",
            100,
        )
        .unwrap());
        assert_eq!(
            load_mailbox_page(&root, "fixture-account", "INBOX", None, 10)
                .unwrap()
                .unwrap()
                .total_cached,
            1
        );
        assert_eq!(
            load_mailbox_page(&root, "fixture-account", "Archive", None, 10)
                .unwrap()
                .unwrap()
                .total_cached,
            1
        );

        remove_message(&root, "fixture-account", "INBOX", 2).unwrap();
        assert_eq!(
            load_mailbox_page(&root, "fixture-account", "INBOX", None, 10)
                .unwrap()
                .unwrap()
                .total_cached,
            0
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_merge_touches_only_result_rows_in_a_large_mailbox() {
        let root = temporary_root("bounded-search-merge");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.uid_validity = Some(77);
        mailbox.messages = vec![fixture_message(30_000)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        let preserved = encrypted_payload_for_test(&root, "fixture-account", "INBOX", 30_000)
            .unwrap()
            .unwrap();
        insert_corrupt_rows(&root, "INBOX", 1, 20_000);

        merge_messages(
            &root,
            "fixture-account",
            "INBOX",
            Some(77),
            "fixture-search",
            &[fixture_message(40_000)],
            25_000,
        )
        .unwrap();

        assert_eq!(message_count(&root, "INBOX"), 20_002);
        assert_eq!(
            load_message(&root, "fixture-account", "INBOX", 40_000)
                .unwrap()
                .unwrap()
                .uid,
            40_000
        );
        assert_eq!(
            encrypted_payload_for_test(&root, "fixture-account", "INBOX", 30_000)
                .unwrap()
                .unwrap(),
            preserved
        );
        assert!(load_message(&root, "fixture-account", "INBOX", 1).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn search_merge_resets_only_the_changed_uid_namespace() {
        let root = temporary_root("search-uidvalidity");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.uid_validity = Some(10);
        mailbox.highest_mod_seq = Some(500);
        mailbox.has_more = true;
        mailbox.messages = vec![fixture_message(10)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();

        merge_messages(
            &root,
            "fixture-account",
            "INBOX",
            Some(11),
            "fixture-search",
            &[fixture_message(11)],
            100,
        )
        .unwrap();

        assert!(load_message(&root, "fixture-account", "INBOX", 10)
            .unwrap()
            .is_none());
        assert_eq!(
            load_message(&root, "fixture-account", "INBOX", 11)
                .unwrap()
                .unwrap()
                .uid,
            11
        );
        let connection = open(&root).unwrap();
        let (metadata, _) = read_metadata(
            &connection,
            &identity_key("fixture-account"),
            &folder_identity_key("fixture-account", "INBOX"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(metadata.uid_validity, Some(11));
        assert_eq!(metadata.highest_mod_seq, None);
        assert!(!metadata.has_more);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn move_is_one_row_transaction_even_with_large_corrupt_neighbors() {
        let root = temporary_root("bounded-move");
        let mut source = CachedMailbox::empty("fixture-account", "INBOX");
        source.messages = vec![fixture_message(30_000)];
        save_mailbox(&root, "fixture-account", &source).unwrap();
        insert_corrupt_rows(&root, "INBOX", 1, 20_000);
        let corrupt_neighbor = encrypted_payload_for_test(&root, "fixture-account", "INBOX", 1)
            .unwrap()
            .unwrap();

        assert!(move_message(
            &root,
            "fixture-account",
            "INBOX",
            30_000,
            "Archive",
            "fixture-move",
            25_000,
        )
        .unwrap());

        assert!(load_message(&root, "fixture-account", "INBOX", 30_000)
            .unwrap()
            .is_none());
        let moved = load_message(&root, "fixture-account", "Archive", 30_000)
            .unwrap()
            .unwrap();
        assert_eq!(moved.folder, "Archive");
        assert_eq!(message_count(&root, "INBOX"), 20_000);
        assert_eq!(message_count(&root, "Archive"), 1);
        assert_eq!(
            encrypted_payload_for_test(&root, "fixture-account", "INBOX", 1)
                .unwrap()
                .unwrap(),
            corrupt_neighbor
        );
        let connection = open(&root).unwrap();
        let (source_metadata, source_revision) = read_metadata(
            &connection,
            &identity_key("fixture-account"),
            &folder_identity_key("fixture-account", "INBOX"),
        )
        .unwrap()
        .unwrap();
        let (target_metadata, target_revision) = read_metadata(
            &connection,
            &identity_key("fixture-account"),
            &folder_identity_key("fixture-account", "Archive"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(source_metadata.oldest_uid, Some(1));
        assert_eq!(target_metadata.oldest_uid, Some(30_000));
        assert_eq!(source_revision, 2);
        assert_eq!(target_revision, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn move_rolls_back_source_delete_when_target_insert_fails() {
        let root = temporary_root("atomic-move-rollback");
        let mut source = CachedMailbox::empty("fixture-account", "INBOX");
        source.messages = vec![fixture_message(30_000)];
        save_mailbox(&root, "fixture-account", &source).unwrap();
        let connection = open(&root).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fixture_reject_move
                 BEFORE INSERT ON messages WHEN NEW.uid = 30000
                 BEGIN SELECT RAISE(ABORT, 'fixture target failure'); END;",
            )
            .unwrap();
        drop(connection);

        assert!(move_message(
            &root,
            "fixture-account",
            "INBOX",
            30_000,
            "Archive",
            "fixture-move",
            100,
        )
        .is_err());

        assert_eq!(
            load_message(&root, "fixture-account", "INBOX", 30_000)
                .unwrap()
                .unwrap()
                .uid,
            30_000
        );
        assert!(!mailbox_exists(&root, "fixture-account", "Archive").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn busy_and_locked_errors_are_not_classified_as_corruption() {
        for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
            let error = anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(!is_sqlite_corruption(&error));
        }
        let corrupt = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        ));
        assert!(is_sqlite_corruption(&corrupt));
        assert!(!is_sqlite_corruption(&backup_maintenance_error(corrupt)));
    }

    #[test]
    fn online_backup_keeps_protected_rows_and_recovers_a_corrupt_primary() {
        let root = temporary_root("backup-recovery");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(9), fixture_message(8)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        refresh_backup(&root).unwrap();

        let backup = fs::read(backup_path(&root)).unwrap();
        assert!(!String::from_utf8_lossy(&backup).contains("subject 9"));
        fs::write(database_path(&root), b"damaged indexed cache").unwrap();

        let recovered = load_mailbox(&root, "fixture-account", "INBOX")
            .unwrap()
            .unwrap();
        assert_eq!(recovered.messages.len(), 2);
        assert_eq!(recovered.messages[0].uid, 9);
        assert!(fs::read_dir(&root).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("mail-index-v1.sqlite3.corrupt-")
        }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_backup_is_preserved_while_the_rebuild_stays_usable() {
        let root = temporary_root("invalid-backup");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(3)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        refresh_backup(&root).unwrap();
        let damage = b"damaged recovery copy";
        fs::write(database_path(&root), b"damaged indexed cache").unwrap();
        fs::write(backup_path(&root), damage).unwrap();

        assert!(load_mailbox(&root, "fixture-account", "INBOX")
            .unwrap()
            .is_none());
        assert_eq!(fs::read(backup_path(&root)).unwrap(), damage);
        assert!(validate_database_file(&database_path(&root)).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_primary_is_restored_from_the_valid_backup() {
        let root = temporary_root("missing-primary");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(6)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        refresh_backup(&root).unwrap();
        fs::remove_file(database_path(&root)).unwrap();

        let recovered = load_message(&root, "fixture-account", "INBOX", 6)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.uid, 6);
        assert!(validate_database_file(&database_path(&root)).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_removal_cannot_be_reversed_by_an_older_backup_generation() {
        let root = temporary_root("account-removal");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(5)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        refresh_backup(&root).unwrap();
        remove_account(&root, "fixture-account").unwrap();

        assert!(!root.join(DATABASE_BACKUP_PREVIOUS_FILE).exists());
        let backup = Connection::open_with_flags(
            backup_path(&root),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let cached_accounts: i64 = backup
            .query_row("SELECT COUNT(*) FROM mailbox_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cached_accounts, 0);
        drop(backup);

        fs::write(database_path(&root), b"damaged indexed cache").unwrap();
        assert!(load_mailbox(&root, "fixture-account", "INBOX")
            .unwrap()
            .is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_readers_share_one_serialized_recovery() {
        let root = temporary_root("concurrent-recovery");
        let mut mailbox = CachedMailbox::empty("fixture-account", "INBOX");
        mailbox.messages = vec![fixture_message(12)];
        save_mailbox(&root, "fixture-account", &mailbox).unwrap();
        refresh_backup(&root).unwrap();
        fs::write(database_path(&root), b"damaged indexed cache").unwrap();

        let readers = (0..8)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || {
                    load_message(&root, "fixture-account", "INBOX", 12)
                        .unwrap()
                        .unwrap()
                        .uid
                })
            })
            .collect::<Vec<_>>();
        for reader in readers {
            assert_eq!(reader.join().unwrap(), 12);
        }
        let quarantine_count = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("mail-index-v1.sqlite3.corrupt-")
            })
            .count();
        assert_eq!(quarantine_count, 1);
        let _ = fs::remove_dir_all(root);
    }
}
