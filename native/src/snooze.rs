use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::mail::CachedMessage;

const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_FILE: &str = "snoozed.bin";
const STORE_BACKUP_FILE: &str = "snoozed.bin.bak";
const STORE_TEMP_FILE: &str = "snoozed.bin.tmp";
const MAX_ITEMS: usize = 1_000;
const MAX_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_FOLDER_BYTES: usize = 512;
const MAX_HEADER_BYTES: usize = 998;
const MAX_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_THREAD_VALUES: usize = 100;
const MAX_THREAD_VALUE_BYTES: usize = 998;
pub const MIN_SNOOZE_LEAD_SECONDS: u64 = 60;
pub const MAX_SNOOZE_AHEAD_SECONDS: u64 = 366 * 24 * 60 * 60;

static SNOOZE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static SCHEDULER_WAKE: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();

fn snooze_guard() -> std::sync::MutexGuard<'static, ()> {
    SNOOZE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("MailGo snooze lock poisoned")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnoozedMessage {
    pub message: CachedMessage,
    pub created_at: u64,
    pub wake_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnoozeStore {
    schema_version: u32,
    items: Vec<SnoozedMessage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnoozeSnapshot {
    pub items: Vec<SnoozedMessage>,
    pub next_wake_at: Option<u64>,
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn item_identity(item: &SnoozedMessage) -> (String, String, u32) {
    (
        item.message.account_id.to_ascii_lowercase(),
        item.message.folder.to_lowercase(),
        item.message.uid,
    )
}

fn validate_header(value: &str, max_bytes: usize, field: &str) -> Result<()> {
    if value.len() > max_bytes || value.chars().any(|character| character == '\0') {
        return Err(anyhow!("snoozed message {field} is outside the safe limit"));
    }
    Ok(())
}

fn validate_thread_value(value: &Option<String>, field: &str) -> Result<()> {
    if let Some(value) = value {
        validate_header(value, MAX_THREAD_VALUE_BYTES, field)?;
    }
    Ok(())
}

fn validate(item: &SnoozedMessage) -> Result<()> {
    let message = &item.message;
    if !crate::valid_account_id(&message.account_id) {
        return Err(anyhow!("invalid snoozed account id"));
    }
    if message.folder.trim().is_empty()
        || message.folder.len() > MAX_FOLDER_BYTES
        || message
            .folder
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
    {
        return Err(anyhow!("invalid snoozed mailbox name"));
    }
    if message.uid == 0 {
        return Err(anyhow!("invalid snoozed message UID"));
    }
    if item.wake_at < item.created_at
        || item.wake_at > item.created_at.saturating_add(MAX_SNOOZE_AHEAD_SECONDS)
    {
        return Err(anyhow!("snooze time is outside the safe range"));
    }
    validate_header(&message.id, MAX_HEADER_BYTES, "id")?;
    validate_header(&message.subject, MAX_HEADER_BYTES, "subject")?;
    validate_header(&message.sender_name, MAX_HEADER_BYTES, "sender name")?;
    validate_header(&message.sender_email, MAX_HEADER_BYTES, "sender address")?;
    validate_header(&message.thread_id, MAX_THREAD_VALUE_BYTES, "thread id")?;
    validate_header(&message.preview, MAX_PREVIEW_BYTES, "preview")?;
    validate_thread_value(&message.message_id, "message id")?;
    validate_thread_value(&message.in_reply_to, "in-reply-to")?;
    if message.references.len() > MAX_THREAD_VALUES {
        return Err(anyhow!("snoozed message has too many thread references"));
    }
    for reference in &message.references {
        validate_header(reference, MAX_THREAD_VALUE_BYTES, "thread reference")?;
    }
    if !message.to.is_empty()
        || !message.cc.is_empty()
        || !message.text_body.is_empty()
        || message.html_body.is_some()
        || !message.attachments.is_empty()
        || message.raw_path.is_some()
    {
        return Err(anyhow!("snoozed message contains non-summary payload"));
    }
    Ok(())
}

fn validate_store(store: &SnoozeStore) -> Result<()> {
    if store.schema_version > STORE_SCHEMA_VERSION || store.items.len() > MAX_ITEMS {
        return Err(anyhow!("unsupported or oversized snooze store"));
    }
    let mut identities = HashSet::with_capacity(store.items.len());
    for item in &store.items {
        validate(item)?;
        if !identities.insert(item_identity(item)) {
            return Err(anyhow!("duplicate snoozed message identity"));
        }
    }
    Ok(())
}

fn empty_store() -> SnoozeStore {
    SnoozeStore {
        schema_version: STORE_SCHEMA_VERSION,
        items: Vec::new(),
    }
}

fn decode(bytes: &[u8]) -> Result<SnoozeStore> {
    if bytes.len() > MAX_STORE_BYTES {
        return Err(anyhow!("snooze store is too large"));
    }
    let decoded = crate::sync::unprotect_cache(bytes).context("decrypt snooze store")?;
    if decoded.len() > MAX_STORE_BYTES {
        return Err(anyhow!("snooze store is too large"));
    }
    let mut store: SnoozeStore = serde_json::from_slice(&decoded).context("parse snooze store")?;
    validate_store(&store)?;
    store.schema_version = STORE_SCHEMA_VERSION;
    Ok(store)
}

fn load(cache_root: &Path) -> Result<SnoozeStore> {
    let path = cache_root.join(STORE_FILE);
    let backup = cache_root.join(STORE_BACKUP_FILE);
    let primary = match fs::read(&path) {
        Ok(bytes) => decode(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("read {}", path.display()))
        }
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    };
    match primary {
        Ok(store) => Ok(store),
        Err(primary_error) => match fs::read(&backup) {
            Ok(bytes) => decode(&bytes)
                .with_context(|| format!("parse {} after {primary_error}", backup.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if path.exists() {
                    Err(primary_error)
                } else {
                    Ok(empty_store())
                }
            }
            Err(error) => Err(error).with_context(|| format!("read {}", backup.display())),
        },
    }
}

fn persist(cache_root: &Path, store: &SnoozeStore) -> Result<()> {
    validate_store(store)?;
    fs::create_dir_all(cache_root).with_context(|| format!("create {}", cache_root.display()))?;
    let payload = serde_json::to_vec(store).context("serialize snooze store")?;
    if payload.len() > MAX_STORE_BYTES {
        return Err(anyhow!("snooze store is too large"));
    }
    let encrypted = crate::sync::protect_cache(&payload).context("encrypt snooze store")?;
    if encrypted.len() > MAX_STORE_BYTES {
        return Err(anyhow!("snooze store is too large"));
    }
    let temporary = cache_root.join(STORE_TEMP_FILE);
    let path = cache_root.join(STORE_FILE);
    let backup = cache_root.join(STORE_BACKUP_FILE);
    fs::write(&temporary, encrypted).context("write snooze store")?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .context("open pending snooze store")?
        .sync_all()
        .context("flush pending snooze store")?;
    if path.exists() {
        let _ = fs::remove_file(&backup);
        fs::rename(&path, &backup).context("backup snooze store")?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::rename(&backup, &path);
        return Err(error).context("commit snooze store");
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn summary(mut message: CachedMessage) -> CachedMessage {
    message.to.clear();
    message.cc.clear();
    message.text_body.clear();
    message.html_body = None;
    message.attachments.clear();
    message.raw_path = None;
    message
}

fn snapshot_at(store: &SnoozeStore, now: u64) -> SnoozeSnapshot {
    let mut items = store
        .items
        .iter()
        .filter(|item| item.wake_at > now)
        .cloned()
        .collect::<Vec<_>>();
    items.sort_by_key(|item| item.wake_at);
    SnoozeSnapshot {
        next_wake_at: items.first().map(|item| item.wake_at),
        items,
    }
}

fn schedule_at(
    cache_root: &Path,
    message: CachedMessage,
    wake_at: u64,
    now: u64,
) -> Result<SnoozeSnapshot> {
    if wake_at < now.saturating_add(MIN_SNOOZE_LEAD_SECONDS)
        || wake_at > now.saturating_add(MAX_SNOOZE_AHEAD_SECONDS)
    {
        return Err(anyhow!(
            "snooze time must be at least one minute and at most one year from now"
        ));
    }
    let _guard = snooze_guard();
    let mut store = load(cache_root)?;
    let item = SnoozedMessage {
        message: summary(message),
        created_at: now,
        wake_at,
    };
    validate(&item)?;
    let identity = item_identity(&item);
    if let Some(existing) = store
        .items
        .iter_mut()
        .find(|existing| item_identity(existing) == identity)
    {
        *existing = item;
    } else {
        if store.items.len() >= MAX_ITEMS {
            return Err(anyhow!("snooze store has reached its 1000-message limit"));
        }
        store.items.push(item);
    }
    persist(cache_root, &store)?;
    let snapshot = snapshot_at(&store, now);
    drop(_guard);
    notify_scheduler();
    Ok(snapshot)
}

pub fn schedule(cache_root: &Path, message: CachedMessage, wake_at: u64) -> Result<SnoozeSnapshot> {
    schedule_at(cache_root, message, wake_at, now_seconds())
}

fn unsnooze_at(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
    now: u64,
) -> Result<(bool, SnoozeSnapshot)> {
    let _guard = snooze_guard();
    let mut store = load(cache_root)?;
    let previous_len = store.items.len();
    store.items.retain(|item| {
        !(item.message.account_id.eq_ignore_ascii_case(account_id)
            && item.message.folder.eq_ignore_ascii_case(folder)
            && item.message.uid == uid)
    });
    let removed = store.items.len() != previous_len;
    if removed {
        persist(cache_root, &store)?;
    }
    let snapshot = snapshot_at(&store, now);
    drop(_guard);
    if removed {
        notify_scheduler();
    }
    Ok((removed, snapshot))
}

pub fn unsnooze(
    cache_root: &Path,
    account_id: &str,
    folder: &str,
    uid: u32,
) -> Result<(bool, SnoozeSnapshot)> {
    unsnooze_at(cache_root, account_id, folder, uid, now_seconds())
}

pub fn snapshot(cache_root: &Path) -> Result<SnoozeSnapshot> {
    let _guard = snooze_guard();
    let store = load(cache_root)?;
    Ok(snapshot_at(&store, now_seconds()))
}

fn release_due_at(cache_root: &Path, now: u64) -> Result<Vec<SnoozedMessage>> {
    let _guard = snooze_guard();
    let mut store = load(cache_root)?;
    let mut due = Vec::new();
    store.items.retain(|item| {
        if item.wake_at <= now {
            due.push(item.clone());
            false
        } else {
            true
        }
    });
    if !due.is_empty() {
        persist(cache_root, &store)?;
    }
    Ok(due)
}

pub fn release_due(cache_root: &Path) -> Result<Vec<SnoozedMessage>> {
    release_due_at(cache_root, now_seconds())
}

fn next_due_delay_at(cache_root: &Path, now: u64) -> Result<Option<Duration>> {
    let _guard = snooze_guard();
    let store = load(cache_root)?;
    Ok(store
        .items
        .iter()
        .map(|item| item.wake_at)
        .min()
        .map(|wake_at| Duration::from_secs(wake_at.saturating_sub(now))))
}

pub fn next_due_delay(cache_root: &Path) -> Result<Option<Duration>> {
    next_due_delay_at(cache_root, now_seconds())
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<usize> {
    let _guard = snooze_guard();
    let mut store = load(cache_root)?;
    let previous_len = store.items.len();
    store
        .items
        .retain(|item| !item.message.account_id.eq_ignore_ascii_case(account_id));
    let removed = previous_len.saturating_sub(store.items.len());
    if removed > 0 {
        persist(cache_root, &store)?;
    }
    drop(_guard);
    if removed > 0 {
        notify_scheduler();
    }
    Ok(removed)
}

pub fn scheduler_generation() -> u64 {
    *SCHEDULER_WAKE
        .get_or_init(|| (Mutex::new(0), Condvar::new()))
        .0
        .lock()
        .expect("MailGo snooze scheduler lock poisoned")
}

pub fn wait_for_scheduler_change(observed: u64, timeout: Duration) {
    let (generation, wake) = SCHEDULER_WAKE.get_or_init(|| (Mutex::new(0), Condvar::new()));
    let generation = generation
        .lock()
        .expect("MailGo snooze scheduler lock poisoned");
    if *generation == observed {
        drop(
            wake.wait_timeout(generation, timeout)
                .expect("MailGo snooze scheduler wait poisoned"),
        );
    }
}

pub fn notify_scheduler() {
    let (generation, wake) = SCHEDULER_WAKE.get_or_init(|| (Mutex::new(0), Condvar::new()));
    let mut generation = generation
        .lock()
        .expect("MailGo snooze scheduler lock poisoned");
    *generation = generation.wrapping_add(1);
    wake.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::SmartCategory;

    fn fixture(account_id: &str, uid: u32) -> CachedMessage {
        CachedMessage {
            id: format!("{account_id}:INBOX:{uid}"),
            message_id: Some(format!("message-{uid}@example.invalid")),
            in_reply_to: None,
            references: Vec::new(),
            thread_id: format!("thread-{uid}"),
            account_id: account_id.into(),
            folder: "INBOX".into(),
            uid,
            subject: format!("Snoozed message {uid}"),
            sender_name: "Sender".into(),
            sender_email: "sender@example.invalid".into(),
            to: vec!["recipient@example.invalid".into()],
            cc: Vec::new(),
            received_at: Some("2026-09-03T08:00:00Z".into()),
            unread: true,
            starred: false,
            category: SmartCategory::Inbox,
            is_ad: false,
            preview: "Safe preview".into(),
            text_body: "body must not be copied".into(),
            html_body: Some("<p>body must not be copied</p>".into()),
            attachments: Vec::new(),
            raw_path: None,
        }
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mailgo-snooze-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn encrypted_snooze_round_trip_is_summary_only_and_deduplicated() {
        let root = test_root("round-trip");
        let first = schedule_at(&root, fixture("account-1", 7), 1_100, 1_000).unwrap();
        assert_eq!(first.items.len(), 1);
        assert!(first.items[0].message.text_body.is_empty());
        assert!(first.items[0].message.html_body.is_none());
        assert!(first.items[0].message.to.is_empty());
        let updated = schedule_at(&root, fixture("account-1", 7), 1_200, 1_000).unwrap();
        assert_eq!(updated.items.len(), 1);
        assert_eq!(updated.items[0].wake_at, 1_200);
        let encrypted = fs::read(root.join(STORE_FILE)).unwrap();
        assert!(!encrypted
            .windows(b"Snoozed message".len())
            .any(|window| window == b"Snoozed message"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn due_release_and_unsnooze_are_deterministic() {
        let root = test_root("release");
        schedule_at(&root, fixture("account-1", 1), 1_060, 1_000).unwrap();
        schedule_at(&root, fixture("account-2", 2), 1_120, 1_000).unwrap();
        assert_eq!(
            next_due_delay_at(&root, 1_010).unwrap(),
            Some(Duration::from_secs(50))
        );
        let due = release_due_at(&root, 1_060).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].message.uid, 1);
        let (removed, snapshot) = unsnooze_at(&root, "account-2", "inbox", 2, 1_070).unwrap();
        assert!(removed);
        assert!(snapshot.items.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_removal_and_time_bounds_are_enforced() {
        let root = test_root("account");
        assert!(schedule_at(&root, fixture("account-1", 1), 1_059, 1_000).is_err());
        assert!(schedule_at(
            &root,
            fixture("account-1", 1),
            1_000 + MAX_SNOOZE_AHEAD_SECONDS + 1,
            1_000
        )
        .is_err());
        schedule_at(&root, fixture("account-1", 1), 1_100, 1_000).unwrap();
        schedule_at(&root, fixture("account-2", 2), 1_100, 1_000).unwrap();
        assert_eq!(remove_account(&root, "account-1").unwrap(), 1);
        assert_eq!(snapshot_at(&load(&root).unwrap(), 1_000).items.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn schedule_wakes_the_shared_scheduler() {
        let root = test_root("wake");
        let generation = scheduler_generation();
        schedule_at(&root, fixture("account-1", 1), 1_100, 1_000).unwrap();
        assert_ne!(scheduler_generation(), generation);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_primary_recovers_from_backup() {
        let root = test_root("backup");
        schedule_at(&root, fixture("account-1", 1), 1_100, 1_000).unwrap();
        fs::rename(root.join(STORE_FILE), root.join(STORE_BACKUP_FILE)).unwrap();
        let recovered = load(&root).unwrap();
        assert_eq!(recovered.items.len(), 1);
        assert_eq!(recovered.items[0].message.uid, 1);
        let _ = fs::remove_dir_all(root);
    }
}
