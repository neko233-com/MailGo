use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mail::{CachedMailbox, CachedMessage, CACHE_SCHEMA_VERSION};

const DATABASE_FILE: &str = "mail-index-v1.sqlite3";
const DATABASE_SCHEMA_VERSION: i64 = 1;
const MAX_PAGE_SIZE: usize = 500;
const MAX_ENCRYPTED_ROW_BYTES: usize = 8 * 1024 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

static INITIALIZED_DATABASES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

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

pub fn database_path(cache_root: &Path) -> PathBuf {
    cache_root.join(DATABASE_FILE)
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

pub fn load_mailbox(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
) -> Result<Option<CachedMailbox>> {
    let connection = open(cache_root)?;
    let account_key = identity_key(account_id);
    let folder_key = folder_identity_key(account_id, folder);
    let Some((metadata, _)) = read_metadata(&connection, &account_key, &folder_key)? else {
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
}

pub fn load_mailbox_page(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    before_uid: Option<u32>,
    limit: usize,
) -> Result<Option<MailboxPage>> {
    let connection = open(cache_root)?;
    let account_key = identity_key(account_id);
    let folder_key = folder_identity_key(account_id, folder);
    let Some((metadata, revision)) = read_metadata(&connection, &account_key, &folder_key)? else {
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
}

pub fn load_message(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<Option<CachedMessage>> {
    let connection = open(cache_root)?;
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
}

pub fn save_mailbox(
    cache_root: &Path,
    account_id: &str,
    mailbox: &CachedMailbox,
) -> Result<PathBuf> {
    if mailbox.account_id != account_id {
        return Err(anyhow!("indexed mail cache account mismatch"));
    }
    let mut connection = open(cache_root)?;
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
            "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
               payload_hash = excluded.payload_hash,
               payload = excluded.payload,
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
    Ok(database_path(cache_root))
}

pub fn save_message(cache_root: &Path, account_id: &str, message: &CachedMessage) -> Result<()> {
    if message.account_id != account_id {
        return Err(anyhow!("indexed mail cache account mismatch"));
    }
    let mut connection = open(cache_root)?;
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
        "INSERT INTO messages(account_key, folder_key, uid, payload_hash, payload, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_key, folder_key, uid) DO UPDATE SET
           payload_hash = excluded.payload_hash,
           payload = excluded.payload,
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
}

pub fn remove_message(cache_root: &Path, account_id: &str, folder: &str, uid: u32) -> Result<()> {
    let mut connection = open(cache_root)?;
    let account_key = identity_key(account_id);
    let folder_key = folder_identity_key(account_id, folder);
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some((mut metadata, _)) = read_metadata(&transaction, &account_key, &folder_key)? else {
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
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<()> {
    let path = database_path(cache_root);
    if !path.exists() {
        return Ok(());
    }
    let connection = open(cache_root)?;
    let account_key = identity_key(account_id);
    connection.execute(
        "DELETE FROM mailbox_meta WHERE account_key = ?1",
        params![account_key],
    )?;
    Ok(())
}

#[cfg(test)]
pub fn encrypted_payload_for_test(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<Option<Vec<u8>>> {
    let connection = open(cache_root)?;
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
}
