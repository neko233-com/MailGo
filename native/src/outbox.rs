use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::providers::ProviderProfile;

const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_FILE: &str = "outbox.bin";
const STORE_BACKUP_FILE: &str = "outbox.bin.bak";
const STORE_TEMP_FILE: &str = "outbox.bin.tmp";
const MAX_MESSAGES: usize = 100;
const MAX_STORE_BYTES: usize = 128 * 1024 * 1024;
const MAX_RECIPIENT_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 10;
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_TOTAL_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
const MAX_FILE_NAME_BYTES: usize = 255;
const MAX_CONTENT_TYPE_BYTES: usize = 128;
const MAX_FAILURE_BYTES: usize = 256;
const MAX_RETRY_ATTEMPTS: u32 = 8;

static OUTBOX_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static FLUSH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

fn outbox_guard() -> std::sync::MutexGuard<'static, ()> {
    OUTBOX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("MailGo outbox lock poisoned")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedAttachment {
    pub file_name: String,
    pub content_type: String,
    #[serde(default)]
    pub content_id: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedMessage {
    pub id: String,
    pub account_id: String,
    pub to: String,
    #[serde(default)]
    pub cc: String,
    #[serde(default)]
    pub bcc: String,
    pub subject: String,
    pub text_body: String,
    #[serde(default)]
    pub html_body: Option<String>,
    #[serde(default)]
    pub attachments: Vec<QueuedAttachment>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub next_attempt_at: u64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutboxStore {
    schema_version: u32,
    messages: Vec<QueuedMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxStatus {
    pub total: usize,
    pub pending: usize,
    pub paused: usize,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FlushSummary {
    pub sent: usize,
    pub retried: usize,
    pub paused: usize,
}

pub fn enqueue(cache_root: &Path, mut message: QueuedMessage) -> Result<QueuedMessage> {
    let _outbox_guard = outbox_guard();
    if message.id.is_empty() {
        message.id = format!("outbox-{:016x}", rand::random::<u64>());
    }
    let timestamp = now_seconds();
    message.created_at = if message.created_at == 0 {
        timestamp
    } else {
        message.created_at
    };
    message.updated_at = timestamp;
    message.attempts = 0;
    message.next_attempt_at = timestamp;
    message.paused = false;
    message.last_error = None;
    validate(&message)?;

    let mut store = load(cache_root)?;
    store.messages.retain(|item| item.id != message.id);
    store.messages.push(message.clone());
    store
        .messages
        .sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    store.messages.truncate(MAX_MESSAGES);
    validate_store(&store)?;
    persist(cache_root, &store)?;
    Ok(message)
}

pub fn status(cache_root: &Path, account_id: &str) -> Result<OutboxStatus> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    let messages = load(cache_root)?
        .messages
        .into_iter()
        .filter(|item| item.account_id == account_id)
        .collect::<Vec<_>>();
    let paused = messages.iter().filter(|item| item.paused).count();
    Ok(OutboxStatus {
        total: messages.len(),
        pending: messages.len().saturating_sub(paused),
        paused,
    })
}

pub fn retry_all(cache_root: &Path, account_id: &str) -> Result<usize> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    let mut store = load(cache_root)?;
    let now = now_seconds();
    let mut changed = 0;
    for message in &mut store.messages {
        if message.account_id != account_id || (!message.paused && message.next_attempt_at <= now) {
            continue;
        }
        message.attempts = 0;
        message.next_attempt_at = now;
        message.paused = false;
        message.last_error = None;
        message.updated_at = now;
        changed += 1;
    }
    if changed > 0 {
        persist(cache_root, &store)?;
    }
    Ok(changed)
}

pub fn remove(cache_root: &Path, account_id: &str, message_id: &str) -> Result<bool> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    validate_message_id(message_id)?;
    let mut store = load(cache_root)?;
    let original_len = store.messages.len();
    store
        .messages
        .retain(|item| !(item.account_id == account_id && item.id == message_id));
    if original_len == store.messages.len() {
        return Ok(false);
    }
    persist(cache_root, &store)?;
    Ok(true)
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<()> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    let mut store = load(cache_root)?;
    let original_len = store.messages.len();
    store.messages.retain(|item| item.account_id != account_id);
    if original_len != store.messages.len() {
        persist(cache_root, &store)?;
    }
    Ok(())
}

pub fn resume_account(cache_root: &Path, account_id: &str) -> Result<usize> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    let mut store = load(cache_root)?;
    let now = now_seconds();
    let mut changed = 0;
    for message in &mut store.messages {
        if message.account_id != account_id || !message.paused {
            continue;
        }
        message.attempts = 0;
        message.next_attempt_at = now;
        message.paused = false;
        message.last_error = None;
        message.updated_at = now;
        changed += 1;
    }
    if changed > 0 {
        persist(cache_root, &store)?;
    }
    Ok(changed)
}

pub fn flush_due(
    cache_root: &Path,
    account_id: &str,
    profile: ProviderProfile,
    email: &str,
    credential: &str,
) -> Result<FlushSummary> {
    if FLUSH_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return Ok(FlushSummary::default());
    }
    struct FlushReset;
    impl Drop for FlushReset {
        fn drop(&mut self) {
            FLUSH_IN_PROGRESS.store(false, Ordering::Release);
        }
    }
    let _flush_reset = FlushReset;
    validate_account_id(account_id)?;
    let now = now_seconds();
    let due = {
        let _outbox_guard = outbox_guard();
        load(cache_root)?
            .messages
            .into_iter()
            .filter(|item| {
                item.account_id == account_id && !item.paused && item.next_attempt_at <= now
            })
            .collect::<Vec<_>>()
    };
    let mut summary = FlushSummary::default();
    for message in due {
        let attachments = message
            .attachments
            .iter()
            .map(|attachment| crate::send::OutgoingAttachment {
                file_name: attachment.file_name.clone(),
                content_type: attachment.content_type.clone(),
                content_id: attachment.content_id.clone(),
                bytes: attachment.bytes.clone(),
            })
            .collect::<Vec<_>>();
        let outgoing = crate::send::OutgoingMessage {
            from: email,
            credential,
            to: &message.to,
            cc: (!message.cc.is_empty()).then_some(message.cc.as_str()),
            bcc: (!message.bcc.is_empty()).then_some(message.bcc.as_str()),
            subject: &message.subject,
            text_body: &message.text_body,
            html_body: message.html_body.as_deref(),
        };
        match crate::send::send_message(profile.clone(), &outgoing, &attachments) {
            Ok(()) => {
                remove(cache_root, account_id, &message.id)?;
                summary.sent += 1;
            }
            Err(error) => {
                let retryable = crate::send::is_retryable_error(&error);
                record_failure(
                    cache_root,
                    account_id,
                    &message.id,
                    retryable,
                    crate::send::retry_after_seconds(&error),
                    if retryable {
                        "网络暂不可用，MailGo 将自动重试"
                    } else if is_authentication_error(&error) {
                        "账户需要重新授权后才能发送"
                    } else {
                        "发送失败，请检查账户或邮件内容"
                    },
                )?;
                if retryable {
                    summary.retried += 1;
                } else {
                    summary.paused += 1;
                }
            }
        }
    }
    Ok(summary)
}

fn record_failure(
    cache_root: &Path,
    account_id: &str,
    message_id: &str,
    retryable: bool,
    retry_after: Option<u64>,
    reason: &str,
) -> Result<()> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    validate_message_id(message_id)?;
    let mut store = load(cache_root)?;
    let now = now_seconds();
    let Some(message) = store
        .messages
        .iter_mut()
        .find(|item| item.account_id == account_id && item.id == message_id)
    else {
        return Ok(());
    };
    message.attempts = message.attempts.saturating_add(1);
    message.updated_at = now;
    message.last_error = Some(reason.chars().take(MAX_FAILURE_BYTES).collect());
    if !retryable || message.attempts >= MAX_RETRY_ATTEMPTS {
        message.paused = true;
        message.next_attempt_at = 0;
    } else {
        let fallback = 30u64.saturating_mul(1u64 << message.attempts.saturating_sub(1));
        let delay = retry_after.unwrap_or(fallback).clamp(30, 3_600);
        message.next_attempt_at = now.saturating_add(delay);
    }
    persist(cache_root, &store)
}

fn is_authentication_error(error: &anyhow::Error) -> bool {
    let message = error
        .chain()
        .map(|source| source.to_string())
        .collect::<Vec<_>>()
        .join(": ")
        .to_ascii_lowercase();
    [
        "authentication",
        "authorization",
        "invalid credential",
        "requires authorization",
        "auth",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn load(cache_root: &Path) -> Result<OutboxStore> {
    let path = cache_root.join(STORE_FILE);
    let backup = cache_root.join(STORE_BACKUP_FILE);
    let store = match fs::read(&path) {
        Ok(bytes) => decode(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(anyhow!("missing")),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    };
    match store {
        Ok(store) => Ok(store),
        Err(primary_error) => match fs::read(&backup) {
            Ok(bytes) => decode(&bytes)
                .with_context(|| format!("parse {} after {}", backup.display(), primary_error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if primary_error.to_string() == "missing" {
                    Ok(OutboxStore {
                        schema_version: STORE_SCHEMA_VERSION,
                        messages: Vec::new(),
                    })
                } else {
                    Err(primary_error)
                }
            }
            Err(error) => Err(error).with_context(|| format!("read {}", backup.display())),
        },
    }
}

fn decode(bytes: &[u8]) -> Result<OutboxStore> {
    if bytes.len() > MAX_STORE_BYTES {
        return Err(anyhow!("outbox store is too large"));
    }
    let decoded = crate::sync::unprotect_cache(bytes).context("decrypt outbox")?;
    if decoded.len() > MAX_STORE_BYTES {
        return Err(anyhow!("outbox store is too large"));
    }
    let store: OutboxStore = serde_json::from_slice(&decoded).context("parse outbox")?;
    validate_store(&store)?;
    Ok(store)
}

fn validate_store(store: &OutboxStore) -> Result<()> {
    if store.schema_version > STORE_SCHEMA_VERSION || store.messages.len() > MAX_MESSAGES {
        return Err(anyhow!("unsupported or oversized outbox store"));
    }
    for message in &store.messages {
        validate(message)?;
    }
    Ok(())
}

fn validate(message: &QueuedMessage) -> Result<()> {
    validate_account_id(&message.account_id)?;
    validate_message_id(&message.id)?;
    validate_text(&message.to, MAX_RECIPIENT_BYTES, "to", false)?;
    validate_text(&message.cc, MAX_RECIPIENT_BYTES, "cc", true)?;
    validate_text(&message.bcc, MAX_RECIPIENT_BYTES, "bcc", true)?;
    validate_text(&message.subject, MAX_SUBJECT_BYTES, "subject", false)?;
    validate_text(&message.text_body, MAX_BODY_BYTES, "text body", true)?;
    if let Some(html) = &message.html_body {
        validate_text(html, MAX_BODY_BYTES, "HTML body", true)?;
    }
    if message.attachments.len() > MAX_ATTACHMENTS {
        return Err(anyhow!("outbox message contains too many attachments"));
    }
    let mut total = 0usize;
    for attachment in &message.attachments {
        if attachment.file_name.trim().is_empty()
            || attachment.file_name.len() > MAX_FILE_NAME_BYTES
            || attachment.file_name == "."
            || attachment.file_name == ".."
            || attachment
                .file_name
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n' | '/' | '\\'))
        {
            return Err(anyhow!("outbox attachment file name is unsafe"));
        }
        if attachment.content_type.is_empty()
            || attachment.content_type.len() > MAX_CONTENT_TYPE_BYTES
            || attachment
                .content_type
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n'))
        {
            return Err(anyhow!("outbox attachment content type is unsafe"));
        }
        if let Some(content_id) = &attachment.content_id {
            if content_id.is_empty()
                || content_id.len() > 128
                || !content_id.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '@')
                })
            {
                return Err(anyhow!("outbox inline attachment content id is unsafe"));
            }
        }
        if attachment.bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(anyhow!("outbox attachment is too large"));
        }
        total = total.saturating_add(attachment.bytes.len());
    }
    if total > MAX_TOTAL_ATTACHMENT_BYTES {
        return Err(anyhow!("outbox attachments exceed the total size limit"));
    }
    if let Some(error) = &message.last_error {
        if error.len() > MAX_FAILURE_BYTES || error.chars().any(|character| character == '\0') {
            return Err(anyhow!("outbox failure detail is unsafe"));
        }
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize, field: &str, allow_empty: bool) -> Result<()> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > max_bytes
        || value.chars().any(|character| {
            character == '\0'
                || (field != "text body"
                    && field != "HTML body"
                    && matches!(character, '\r' | '\n'))
        })
    {
        return Err(anyhow!(
            "outbox field {field} exceeds the safe size or character limit"
        ));
    }
    Ok(())
}

fn validate_message_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(anyhow!("invalid outbox message id"));
    }
    Ok(())
}

fn validate_account_id(value: &str) -> Result<()> {
    if !crate::valid_account_id(value) {
        return Err(anyhow!("invalid account id"));
    }
    Ok(())
}

fn persist(cache_root: &Path, store: &OutboxStore) -> Result<()> {
    validate_store(store)?;
    fs::create_dir_all(cache_root).with_context(|| format!("create {}", cache_root.display()))?;
    let payload = serde_json::to_vec(store).context("serialize outbox")?;
    if payload.len() > MAX_STORE_BYTES {
        return Err(anyhow!("outbox store is too large"));
    }
    let encrypted = crate::sync::protect_cache(&payload).context("encrypt outbox")?;
    let temporary = cache_root.join(STORE_TEMP_FILE);
    let path = cache_root.join(STORE_FILE);
    let backup = cache_root.join(STORE_BACKUP_FILE);
    fs::write(&temporary, encrypted).context("write outbox")?;
    if path.exists() {
        let _ = fs::remove_file(&backup);
        fs::rename(&path, &backup).context("backup outbox")?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::rename(&backup, &path);
        return Err(error).context("commit outbox");
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{profile_for, ProviderKind};

    fn fixture() -> QueuedMessage {
        QueuedMessage {
            id: "outbox-fixture".into(),
            account_id: "account-1".into(),
            to: "person@example.com".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Queued message".into(),
            text_body: "body".into(),
            html_body: Some("<p>body</p>".into()),
            attachments: vec![QueuedAttachment {
                file_name: "hello.txt".into(),
                content_type: "text/plain".into(),
                content_id: None,
                bytes: b"hello".to_vec(),
            }],
            created_at: 1,
            updated_at: 1,
            attempts: 0,
            next_attempt_at: 1,
            paused: false,
            last_error: None,
        }
    }

    #[test]
    fn encrypted_outbox_round_trips_and_filters_by_account() {
        let root = std::env::temp_dir().join(format!("mailgo-outbox-test-{}", std::process::id()));
        let saved = enqueue(&root, fixture()).expect("enqueue message");
        assert_eq!(status(&root, "account-1").unwrap().total, 1);
        assert_eq!(status(&root, "account-2").unwrap().total, 0);
        assert!(remove(&root, "account-1", &saved.id).unwrap());
        assert_eq!(status(&root, "account-1").unwrap().total, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_unsafe_outbox_content() {
        let mut message = fixture();
        message.id = "../outbox".into();
        assert!(enqueue(std::path::Path::new("."), message).is_err());

        let mut message = fixture();
        message.attachments[0].file_name = "..\\secret.txt".into();
        assert!(enqueue(std::path::Path::new("."), message).is_err());
    }

    #[test]
    fn retry_all_unpauses_only_paused_messages() {
        let root =
            std::env::temp_dir().join(format!("mailgo-outbox-resume-{}", std::process::id()));
        let mut store = OutboxStore {
            schema_version: STORE_SCHEMA_VERSION,
            messages: vec![QueuedMessage {
                paused: true,
                last_error: Some("账户需要重新授权后才能发送".into()),
                ..fixture()
            }],
        };
        persist(&root, &store).expect("persist paused fixture");
        assert_eq!(status(&root, "account-1").unwrap().paused, 1);
        assert_eq!(retry_all(&root, "account-1").unwrap(), 1);
        assert_eq!(status(&root, "account-1").unwrap().paused, 0);
        store.messages[0].paused = false;
        store.messages[0].next_attempt_at = now_seconds();
        persist(&root, &store).expect("persist due fixture");
        assert_eq!(retry_all(&root, "account-1").unwrap(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retry_classification_keeps_authentication_failures_out_of_auto_retry() {
        assert!(!crate::send::is_retryable_error(&anyhow!(
            "SMTP authentication failed"
        )));
        assert!(crate::send::is_retryable_error(&anyhow!(
            "SMTP connection timed out"
        )));
        assert!(is_authentication_error(
            &anyhow!("SMTP authentication failed").context("send message")
        ));
        let _ = profile_for(ProviderKind::Google);
    }
}
