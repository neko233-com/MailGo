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
const DATABASE_SCHEMA_VERSION: i64 = 2;
const MAX_PAGE_SIZE: usize = 500;
const MAX_ENCRYPTED_ROW_BYTES: usize = 8 * 1024 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

static INITIALIZED_DATABASES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static DATABASE_ACCESS: OnceLock<RwLock<()>> = OnceLock::new();
static BACKUP_ACCESS: OnceLock<Mutex<()>> = OnceLock::new();
static SEARCH_KEYS: OnceLock<Mutex<HashMap<PathBuf, [u8; SEARCH_KEY_BYTES]>>> = OnceLock::new();
static SEARCH_INDEX_RUNNING: AtomicBool = AtomicBool::new(false);
static SEARCH_INDEX_REQUESTED: AtomicBool = AtomicBool::new(false);

type HmacSha256 = Hmac<Sha256>;

struct SearchIndexerRun;

impl Drop for SearchIndexerRun {
    fn drop(&mut self) {
        SEARCH_INDEX_RUNNING.store(false, Ordering::Release);
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

fn blind_search_term(key: &[u8; SEARCH_KEY_BYTES], gram: &str) -> Result<[u8; 32]> {
    let mut mac = HmacSha256::new_from_slice(key).context("initialize local search HMAC")?;
    mac.update(b"mailgo-search-v1\0");
    mac.update(gram.as_bytes());
    Ok(mac.finalize().into_bytes().into())
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

fn query_search_terms(key: &[u8; SEARCH_KEY_BYTES], words: &[String]) -> Result<Vec<[u8; 32]>> {
    search_grams(words, true, MAX_SEARCH_QUERY_TERMS)
        .iter()
        .map(|gram| blind_search_term(key, gram))
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
         WHERE type = 'table' AND name IN ('mailbox_meta', 'messages')",
        [],
        |row| row.get(0),
    )?;
    Ok(required_tables == 2)
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
            revision INTEGER NOT NULL DEFAULT 1,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (account_key, folder_key)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS messages (
            account_key BLOB NOT NULL,
            folder_key BLOB NOT NULL,
            uid INTEGER NOT NULL CHECK (uid > 0 AND uid <= 4294967295),
            payload_hash BLOB NOT NULL,
            payload BLOB NOT NULL,
            search_version INTEGER NOT NULL DEFAULT 0,
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
    let has_search_version = {
        let mut statement = connection.prepare("PRAGMA table_info(messages)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = false;
        for column in columns {
            if column?.eq_ignore_ascii_case("search_version") {
                found = true;
                break;
            }
        }
        found
    };
    if !has_search_version {
        connection.execute(
            "ALTER TABLE messages ADD COLUMN search_version INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
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
        CREATE TABLE IF NOT EXISTS search_index_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            key_fingerprint BLOB NOT NULL,
            index_version INTEGER NOT NULL
        );",
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
    let encrypted = crate::sync::protect_cache(&serialized)?;
    if encrypted.len() > MAX_ENCRYPTED_ROW_BYTES {
        return Err(anyhow!("encrypted indexed mail cache row is too large"));
    }
    Ok((encrypted, digest))
}

fn decrypt_json<T: for<'de> Deserialize<'de>>(encrypted: &[u8], kind: &str) -> Result<T> {
    if encrypted.len() > MAX_ENCRYPTED_ROW_BYTES {
        return Err(anyhow!("encrypted indexed mail cache {kind} is too large"));
    }
    let serialized = crate::sync::unprotect_cache(encrypted)
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
    transaction.execute(
        "UPDATE messages SET search_version = 0 WHERE search_version != 0",
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
    terms: &[[u8; 32]],
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
        for term in terms {
            insert.execute(params![term, account_key, folder_key, uid])?;
        }
    }
    connection.execute(
        "UPDATE messages SET search_version = ?4
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
                 WHERE search_version != ?1
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
                    message_search_terms(&key, &message)
                });
            match terms {
                Ok(terms) => replace_message_search_terms(
                    &transaction,
                    account_key,
                    folder_key,
                    *uid,
                    &terms,
                )?,
                Err(error) => {
                    // Search is a secondary index. Mark one unreadable row complete with no terms
                    // so it cannot stall every later batch or prevent healthy mail from being found.
                    tracing::warn!(error = %error, "skipped one unreadable message while rebuilding local search");
                    replace_message_search_terms(&transaction, account_key, folder_key, *uid, &[])?;
                }
            }
        }
        let has_more: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE search_version != ?1 LIMIT 1)",
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

pub fn load_mailbox_page(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    before_uid: Option<u32>,
    limit: usize,
) -> Result<Option<MailboxPage>> {
    with_recovery(cache_root, |connection| {
        let account_key = identity_key(account_id);
        let folder_key = folder_identity_key(account_id, folder);
        let Some((metadata, revision)) = read_metadata(connection, &account_key, &folder_key)?
        else {
            return Ok(None);
        };
        validate_identity(&metadata, account_id, folder)?;
        let page_size = limit.clamp(1, MAX_PAGE_SIZE);
        let before = before_uid.map(i64::from);
        let mut statement = connection.prepare(
            "SELECT uid, payload FROM messages
             WHERE account_key = ?1 AND folder_key = ?2
               AND (?3 IS NULL OR uid < ?3)
             ORDER BY uid DESC
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![account_key, folder_key, before, (page_size + 1) as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;
        let mut encrypted_rows = Vec::with_capacity(page_size + 1);
        for row in rows {
            encrypted_rows.push(row?);
        }
        let local_has_more = encrypted_rows.len() > page_size;
        encrypted_rows.truncate(page_size);
        let mut messages = Vec::with_capacity(encrypted_rows.len());
        for (uid, payload) in encrypted_rows {
            let uid = u32::try_from(uid).context("indexed message UID is out of range")?;
            messages.push(decrypt_message(&payload, account_id, folder, uid)?);
        }
        let total_cached: i64 = connection.query_row(
            "SELECT COUNT(*) FROM messages WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| row.get(0),
        )?;
        let remote_has_more = metadata.has_more;
        let mut mailbox = metadata.into_mailbox(messages);
        mailbox.oldest_uid = mailbox.messages.iter().map(|message| message.uid).min();
        mailbox.has_more = local_has_more || remote_has_more;
        Ok(Some(MailboxPage {
            mailbox,
            local_has_more,
            remote_has_more,
            total_cached: total_cached.max(0) as usize,
            revision,
        }))
    })
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
        let (metadata_payload, _) = encrypt_json(&MailboxMetadata::from(mailbox))?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO mailbox_meta(account_key, folder_key, payload, revision, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(account_key, folder_key) DO UPDATE SET
               payload = excluded.payload,
               revision = mailbox_meta.revision + 1,
               updated_at = excluded.updated_at",
            params![account_key, folder_key, metadata_payload, now],
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
        let current_uids = mailbox
            .messages
            .iter()
            .map(|message| i64::from(message.uid))
            .collect::<HashSet<_>>();
        {
            let mut delete = transaction.prepare(
                "DELETE FROM messages WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
            )?;
            for uid in existing.keys().filter(|uid| !current_uids.contains(uid)) {
                delete.execute(params![account_key, folder_key, uid])?;
            }
        }
        {
            let mut upsert = transaction.prepare(
                "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload_hash = excluded.payload_hash,
                   payload = excluded.payload,
                   search_version = 0,
                   updated_at = excluded.updated_at",
            )?;
            for message in &mailbox.messages {
                if message.account_id != account_id
                    || !message.folder.eq_ignore_ascii_case(&mailbox.folder)
                {
                    return Err(anyhow!("indexed message cache identity mismatch"));
                }
                let serialized = serde_json::to_vec(message)?;
                if serialized.len() > MAX_ENCRYPTED_ROW_BYTES {
                    return Err(anyhow!("indexed message cache row is too large"));
                }
                let digest = Sha256::digest(&serialized).to_vec();
                let uid = i64::from(message.uid);
                if existing.get(&uid).is_some_and(|stored| stored == &digest) {
                    continue;
                }
                let payload = crate::sync::protect_cache(&serialized)?;
                if payload.len() > MAX_ENCRYPTED_ROW_BYTES {
                    return Err(anyhow!("encrypted indexed message cache row is too large"));
                }
                upsert.execute(params![account_key, folder_key, uid, digest, payload, now])?;
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
        let (payload, digest) = encrypt_json(message)?;
        transaction.execute(
            "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
             ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
               payload_hash = excluded.payload_hash,
               payload = excluded.payload,
               search_version = 0,
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
            let mut upsert = transaction.prepare(
                "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                 ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
                   payload_hash = excluded.payload_hash,
                   payload = excluded.payload,
                   search_version = 0,
                   updated_at = excluded.updated_at
                 WHERE messages.payload_hash != excluded.payload_hash",
            )?;
            for message in messages {
                let (payload, digest) = encrypt_json(message)?;
                upsert.execute(params![
                    account_key,
                    folder_key,
                    i64::from(message.uid),
                    digest,
                    payload,
                    now
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
        let oldest: Option<i64> = transaction.query_row(
            "SELECT MIN(uid) FROM messages WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| row.get(0),
        )?;
        metadata.oldest_uid = oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        let (metadata_payload, _) = encrypt_json(&metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1, updated_at = ?4
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key, metadata_payload, now],
        )?;
        transaction.commit()?;
        Ok(())
    })
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
        let (message_payload, message_digest) = encrypt_json(&message)?;
        transaction.execute(
            "DELETE FROM messages
             WHERE account_key = ?1 AND folder_key = ?2 AND uid = ?3",
            params![account_key, source_folder_key, i64::from(uid)],
        )?;
        transaction.execute(
            "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, search_version, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
             ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
               payload_hash = excluded.payload_hash,
               payload = excluded.payload,
               search_version = 0,
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
            "DELETE FROM messages
             WHERE account_key = ?1 AND folder_key = ?2 AND uid IN (
               SELECT uid FROM messages
               WHERE account_key = ?1 AND folder_key = ?2
               ORDER BY uid DESC LIMIT -1 OFFSET ?3
             )",
            params![account_key, target_folder_key, max_messages],
        )?;

        let source_oldest: Option<i64> = transaction.query_row(
            "SELECT MIN(uid) FROM messages WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, source_folder_key],
            |row| row.get(0),
        )?;
        source_metadata.oldest_uid = source_oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        source_metadata.synced_at = synced_at.to_string();
        let (source_metadata_payload, _) = encrypt_json(&source_metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1, updated_at = ?4
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, source_folder_key, source_metadata_payload, now],
        )?;

        let target_oldest: Option<i64> = transaction.query_row(
            "SELECT MIN(uid) FROM messages WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, target_folder_key],
            |row| row.get(0),
        )?;
        target_metadata.oldest_uid = target_oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        target_metadata.synced_at = synced_at.to_string();
        let (target_metadata_payload, _) = encrypt_json(&target_metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1, updated_at = ?4
             WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, target_folder_key, target_metadata_payload, now],
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
        let oldest: Option<i64> = transaction.query_row(
            "SELECT MIN(uid) FROM messages WHERE account_key = ?1 AND folder_key = ?2",
            params![account_key, folder_key],
            |row| row.get(0),
        )?;
        metadata.oldest_uid = oldest
            .map(|value| u32::try_from(value).context("indexed message UID is out of range"))
            .transpose()?;
        metadata.synced_at = format!("unix:{}", now_epoch_millis() / 1000);
        let (metadata_payload, _) = encrypt_json(&metadata)?;
        transaction.execute(
            "UPDATE mailbox_meta SET payload = ?3, revision = revision + 1, updated_at = ?4
             WHERE account_key = ?1 AND folder_key = ?2",
            params![
                account_key,
                folder_key,
                metadata_payload,
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
        let search_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('message_search_terms', 'search_index_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_version, DATABASE_SCHEMA_VERSION);
        assert_eq!(search_column, 1);
        assert_eq!(search_tables, 2);
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
    fn blind_index_and_protected_key_never_store_search_plaintext() {
        let root = temporary_root("search-privacy");
        let plaintext = "mailgoprivacycanarytoken";
        let mut message = fixture_message(5);
        message.subject = plaintext.into();
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
        assert!(!original_terms.is_empty());

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
        assert_ne!(original_terms, rotated_terms);
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
