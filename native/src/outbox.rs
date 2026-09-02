use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::providers::ProviderProfile;

const STORE_SCHEMA_VERSION: u32 = 2;
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
const MAX_PREVIEW_CHARS: usize = 240;
const MAX_RETRY_ATTEMPTS: u32 = 8;
pub const MAX_UNDO_SEND_SECONDS: u64 = 30;
pub const MIN_SCHEDULE_LEAD_SECONDS: u64 = 60;
pub const MAX_SCHEDULE_AHEAD_SECONDS: u64 = 366 * 24 * 60 * 60;

static OUTBOX_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static IN_FLIGHT_IDS: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
static FLUSHING_ACCOUNTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static SCHEDULER_WAKE: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();

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
    #[serde(default)]
    pub draft_id: Option<String>,
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
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<QueuedAttachment>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub next_attempt_at: u64,
    #[serde(default)]
    pub scheduled_at: Option<u64>,
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
    pub scheduled: usize,
    pub user_scheduled: usize,
    pub undoable: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxAttachmentSummary {
    pub file_name: String,
    pub content_type: String,
    pub size: u64,
    pub inline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutboxItemState {
    Scheduled,
    Pending,
    Retrying,
    Paused,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxListItem {
    pub id: String,
    pub account_id: String,
    pub draft_id: Option<String>,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub preview: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub next_attempt_at: u64,
    pub scheduled_at: Option<u64>,
    pub attempts: u32,
    pub state: OutboxItemState,
    pub last_error: Option<String>,
    pub attachments: Vec<OutboxAttachmentSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxSnapshot {
    pub status: OutboxStatus,
    pub items: Vec<OutboxListItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecallOutboxStatus {
    Recalled,
    Missing,
    TooLate,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallOutboxResult {
    pub status: RecallOutboxStatus,
    pub draft: Option<crate::drafts::Draft>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiscardOutboxStatus {
    Discarded,
    Missing,
    TooLate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetryOutboxStatus {
    Retried,
    Missing,
    TooLate,
}

struct InFlightGuard {
    keys: Vec<(String, String)>,
}

struct FlushAccountGuard {
    account_id: String,
}

impl FlushAccountGuard {
    fn try_acquire(account_id: &str) -> Option<Self> {
        let mut flushing = FLUSHING_ACCOUNTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("MailGo outbox account-flush lock poisoned");
        flushing.insert(account_id.to_string()).then(|| Self {
            account_id: account_id.to_string(),
        })
    }
}

impl Drop for FlushAccountGuard {
    fn drop(&mut self) {
        FLUSHING_ACCOUNTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("MailGo outbox account-flush lock poisoned")
            .remove(&self.account_id);
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut in_flight = IN_FLIGHT_IDS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("MailGo outbox in-flight lock poisoned");
        for key in &self.keys {
            in_flight.remove(key);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CancelScheduledResult {
    Cancelled,
    Missing,
    TooLate,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FlushSummary {
    pub sent: usize,
    pub retried: usize,
    pub paused: usize,
    pub authentication_failed: bool,
}

pub fn enqueue(cache_root: &Path, message: QueuedMessage) -> Result<QueuedMessage> {
    enqueue_with_delay(cache_root, message, 0)
}

pub fn enqueue_with_delay(
    cache_root: &Path,
    message: QueuedMessage,
    delay_seconds: u64,
) -> Result<QueuedMessage> {
    let next_attempt_at = now_seconds().saturating_add(delay_seconds.min(MAX_UNDO_SEND_SECONDS));
    enqueue_for(cache_root, message, next_attempt_at, None)
}

pub fn enqueue_at(
    cache_root: &Path,
    message: QueuedMessage,
    scheduled_at: u64,
) -> Result<QueuedMessage> {
    let now = now_seconds();
    if scheduled_at < now.saturating_add(MIN_SCHEDULE_LEAD_SECONDS) {
        return Err(anyhow!(
            "scheduled send must be at least one minute in the future"
        ));
    }
    if scheduled_at > now.saturating_add(MAX_SCHEDULE_AHEAD_SECONDS) {
        return Err(anyhow!(
            "scheduled send cannot be more than one year in the future"
        ));
    }
    enqueue_for(cache_root, message, scheduled_at, Some(scheduled_at))
}

fn enqueue_for(
    cache_root: &Path,
    mut message: QueuedMessage,
    next_attempt_at: u64,
    scheduled_at: Option<u64>,
) -> Result<QueuedMessage> {
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
    message.next_attempt_at = next_attempt_at;
    message.scheduled_at = scheduled_at;
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
    notify_scheduler();
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
    Ok(status_for(&messages, now_seconds()))
}

pub fn snapshot(cache_root: &Path, account_id: &str) -> Result<OutboxSnapshot> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    let mut messages = load(cache_root)?
        .messages
        .into_iter()
        .filter(|item| item.account_id == account_id)
        .collect::<Vec<_>>();
    messages.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
    let now = now_seconds();
    let status = status_for(&messages, now);
    let items = messages
        .into_iter()
        .map(|message| OutboxListItem {
            id: message.id,
            account_id: message.account_id,
            draft_id: message.draft_id,
            to: message.to,
            cc: message.cc,
            bcc: message.bcc,
            subject: message.subject,
            preview: bounded_preview(&message.text_body),
            created_at: message.created_at,
            updated_at: message.updated_at,
            next_attempt_at: message.next_attempt_at,
            scheduled_at: message.scheduled_at,
            attempts: message.attempts,
            state: if message.paused {
                OutboxItemState::Paused
            } else if message.attempts > 0 {
                OutboxItemState::Retrying
            } else if message.next_attempt_at > now {
                OutboxItemState::Scheduled
            } else {
                OutboxItemState::Pending
            },
            last_error: message.last_error,
            attachments: message
                .attachments
                .into_iter()
                .map(|attachment| OutboxAttachmentSummary {
                    file_name: attachment.file_name,
                    content_type: attachment.content_type,
                    size: u64::try_from(attachment.bytes.len()).unwrap_or(u64::MAX),
                    inline: attachment.content_id.is_some(),
                })
                .collect(),
        })
        .collect();
    Ok(OutboxSnapshot { status, items })
}

fn status_for(messages: &[QueuedMessage], now: u64) -> OutboxStatus {
    let paused = messages.iter().filter(|item| item.paused).count();
    let scheduled = messages
        .iter()
        .filter(|item| !item.paused && item.attempts == 0 && item.next_attempt_at > now)
        .count();
    let user_scheduled = messages
        .iter()
        .filter(|item| {
            !item.paused
                && item.attempts == 0
                && item.next_attempt_at > now
                && item.scheduled_at.is_some()
        })
        .count();
    OutboxStatus {
        total: messages.len(),
        pending: messages.len().saturating_sub(paused),
        paused,
        scheduled,
        user_scheduled,
        undoable: scheduled.saturating_sub(user_scheduled),
    }
}

fn bounded_preview(value: &str) -> String {
    let mut preview = String::with_capacity(value.len().min(MAX_PREVIEW_CHARS));
    let mut whitespace = false;
    let mut length = 0usize;
    for character in value.chars() {
        if character.is_whitespace() {
            whitespace = !preview.is_empty();
            continue;
        }
        if whitespace && length < MAX_PREVIEW_CHARS {
            preview.push(' ');
            length += 1;
        }
        whitespace = false;
        if length >= MAX_PREVIEW_CHARS {
            break;
        }
        preview.push(character);
        length += 1;
    }
    if preview.is_empty() {
        "无纯文本摘要".to_string()
    } else {
        preview
    }
}

pub fn recall_to_draft(
    cache_root: &Path,
    account_id: &str,
    message_id: &str,
) -> Result<RecallOutboxResult> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    validate_message_id(message_id)?;
    let mut store = load(cache_root)?;
    let Some(index) = store
        .messages
        .iter()
        .position(|item| item.account_id == account_id && item.id == message_id)
    else {
        return Ok(RecallOutboxResult {
            status: RecallOutboxStatus::Missing,
            draft: None,
        });
    };
    if is_in_flight(account_id, message_id) {
        return Ok(RecallOutboxResult {
            status: RecallOutboxStatus::TooLate,
            draft: None,
        });
    }

    let message = store.messages[index].clone();
    let existing = crate::drafts::list(cache_root, account_id)?
        .into_iter()
        .find(|draft| message.draft_id.as_deref() == Some(draft.id.as_str()));
    let (draft, created) = if let Some(draft) = existing {
        (draft, false)
    } else {
        let saved = crate::drafts::save(
            cache_root,
            crate::drafts::Draft {
                id: message.draft_id.clone().unwrap_or_default(),
                account_id: account_id.to_string(),
                to: message.to.clone(),
                cc: message.cc.clone(),
                bcc: message.bcc.clone(),
                subject: message.subject.clone(),
                body: message.text_body.clone(),
                html_mode: message.html_body.is_some(),
                html_body: message.html_body.clone(),
                in_reply_to: message.in_reply_to.clone(),
                references: message.references.clone(),
                attachments: Vec::new(),
                updated_at: 0,
            },
        )?;
        let saved_id = saved.id.clone();
        let reconstructed =
            message
                .attachments
                .iter()
                .enumerate()
                .try_fold(saved, |_, (index, attachment)| {
                    crate::drafts::attach(
                        cache_root,
                        account_id,
                        &saved_id,
                        crate::drafts::NewDraftAttachment {
                            id: format!("recall-{index}-{:016x}", rand::random::<u64>()),
                            file_name: attachment.file_name.clone(),
                            content_type: attachment.content_type.clone(),
                            content_id: attachment.content_id.clone(),
                        },
                        &attachment.bytes,
                    )
                });
        match reconstructed {
            Ok(draft) => (draft, true),
            Err(error) => {
                let _ = crate::drafts::remove(cache_root, account_id, &saved_id);
                return Err(error).context("reconstruct queued draft attachments");
            }
        }
    };

    store.messages.remove(index);
    if let Err(error) = persist(cache_root, &store) {
        if created {
            let _ = crate::drafts::remove(cache_root, account_id, &draft.id);
        }
        return Err(error);
    }
    notify_scheduler();
    Ok(RecallOutboxResult {
        status: RecallOutboxStatus::Recalled,
        draft: Some(draft),
    })
}

pub fn discard_queued(
    cache_root: &Path,
    account_id: &str,
    message_id: &str,
) -> Result<DiscardOutboxStatus> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    validate_message_id(message_id)?;
    let mut store = load(cache_root)?;
    let Some(index) = store
        .messages
        .iter()
        .position(|item| item.account_id == account_id && item.id == message_id)
    else {
        return Ok(DiscardOutboxStatus::Missing);
    };
    if is_in_flight(account_id, message_id) {
        return Ok(DiscardOutboxStatus::TooLate);
    }
    let draft_id = store.messages[index].draft_id.clone();
    store.messages.remove(index);
    persist(cache_root, &store)?;
    notify_scheduler();
    if let Some(draft_id) = draft_id {
        if let Err(error) = crate::drafts::remove(cache_root, account_id, &draft_id) {
            tracing::warn!(account_id = %account_id, "could not remove discarded outbox draft: {error}");
        }
    }
    Ok(DiscardOutboxStatus::Discarded)
}

fn is_in_flight(account_id: &str, message_id: &str) -> bool {
    IN_FLIGHT_IDS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("MailGo outbox in-flight lock poisoned")
        .contains(&(account_id.to_string(), message_id.to_string()))
}

pub fn cancel_scheduled(
    cache_root: &Path,
    account_id: &str,
    message_id: &str,
) -> Result<CancelScheduledResult> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    validate_message_id(message_id)?;
    let mut store = load(cache_root)?;
    let now = now_seconds();
    let Some(index) = store
        .messages
        .iter()
        .position(|item| item.account_id == account_id && item.id == message_id)
    else {
        return Ok(CancelScheduledResult::Missing);
    };
    let message = &store.messages[index];
    if message.paused || message.attempts > 0 || message.next_attempt_at <= now {
        return Ok(CancelScheduledResult::TooLate);
    }
    store.messages.remove(index);
    persist(cache_root, &store)?;
    notify_scheduler();
    Ok(CancelScheduledResult::Cancelled)
}

pub fn retry_all(cache_root: &Path, account_id: &str) -> Result<usize> {
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
        message.scheduled_at = None;
        message.paused = false;
        message.last_error = None;
        message.updated_at = now;
        changed += 1;
    }
    if changed > 0 {
        persist(cache_root, &store)?;
        notify_scheduler();
    }
    Ok(changed)
}

pub fn retry_queued(
    cache_root: &Path,
    account_id: &str,
    message_id: &str,
) -> Result<RetryOutboxStatus> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    validate_message_id(message_id)?;
    let mut store = load(cache_root)?;
    let Some(message) = store
        .messages
        .iter_mut()
        .find(|item| item.account_id == account_id && item.id == message_id)
    else {
        return Ok(RetryOutboxStatus::Missing);
    };
    if is_in_flight(account_id, message_id) {
        return Ok(RetryOutboxStatus::TooLate);
    }
    let now = now_seconds();
    message.attempts = 0;
    message.next_attempt_at = now;
    message.scheduled_at = None;
    message.paused = false;
    message.last_error = None;
    message.updated_at = now;
    persist(cache_root, &store)?;
    notify_scheduler();
    Ok(RetryOutboxStatus::Retried)
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
    notify_scheduler();
    Ok(true)
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<()> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    if IN_FLIGHT_IDS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .expect("MailGo outbox in-flight lock poisoned")
        .iter()
        .any(|(in_flight_account, _)| in_flight_account == account_id)
    {
        return Err(anyhow!(
            "account has an outbox message in flight; retry removal after sending finishes"
        ));
    }
    let mut store = load(cache_root)?;
    let original_len = store.messages.len();
    store.messages.retain(|item| item.account_id != account_id);
    if original_len != store.messages.len() {
        persist(cache_root, &store)?;
        notify_scheduler();
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
        message.next_attempt_at = message.scheduled_at.filter(|at| *at > now).unwrap_or(now);
        message.paused = false;
        message.last_error = None;
        message.updated_at = now;
        changed += 1;
    }
    if changed > 0 {
        persist(cache_root, &store)?;
        notify_scheduler();
    }
    Ok(changed)
}

pub fn pause_account(cache_root: &Path, account_id: &str, reason: &str) -> Result<usize> {
    let _outbox_guard = outbox_guard();
    validate_account_id(account_id)?;
    let mut store = load(cache_root)?;
    let now = now_seconds();
    let safe_reason = reason.chars().take(MAX_FAILURE_BYTES).collect::<String>();
    let mut changed = 0;
    for message in &mut store.messages {
        if message.account_id != account_id || message.paused {
            continue;
        }
        message.paused = true;
        message.next_attempt_at = 0;
        message.last_error = Some(safe_reason.clone());
        message.updated_at = now;
        changed += 1;
    }
    if changed > 0 {
        persist(cache_root, &store)?;
        notify_scheduler();
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
    validate_account_id(account_id)?;
    let Some(_flush_account_guard) = FlushAccountGuard::try_acquire(account_id) else {
        return Ok(FlushSummary::default());
    };
    let now = now_seconds();
    let (due, _in_flight_guard) = {
        let _outbox_guard = outbox_guard();
        let mut in_flight = IN_FLIGHT_IDS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("MailGo outbox in-flight lock poisoned");
        let due = load(cache_root)?
            .messages
            .into_iter()
            .filter(|item| {
                item.account_id == account_id && !item.paused && item.next_attempt_at <= now
            })
            .filter(|item| in_flight.insert((item.account_id.clone(), item.id.clone())))
            .collect::<Vec<_>>();
        let guard = InFlightGuard {
            keys: due
                .iter()
                .map(|item| (item.account_id.clone(), item.id.clone()))
                .collect(),
        };
        (due, guard)
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
            in_reply_to: message.in_reply_to.as_deref(),
            references: &message.references,
        };
        match crate::send::send_message(profile.clone(), &outgoing, &attachments) {
            Ok(()) => {
                remove(cache_root, account_id, &message.id)?;
                if let Some(draft_id) = message.draft_id.as_deref() {
                    if let Err(error) = crate::drafts::remove(cache_root, account_id, draft_id) {
                        tracing::warn!(account_id = %account_id, "could not remove delivered outbox draft: {error}");
                    }
                }
                summary.sent += 1;
            }
            Err(error) => {
                let retryable = crate::send::is_retryable_error(&error);
                let authentication_failed = is_authentication_error(&error);
                record_failure(
                    cache_root,
                    account_id,
                    &message.id,
                    retryable,
                    crate::send::retry_after_seconds(&error),
                    if retryable {
                        "网络暂不可用，MailGo 将自动重试"
                    } else if authentication_failed {
                        "账户需要重新授权后才能发送"
                    } else {
                        "发送失败，请检查账户或邮件内容"
                    },
                )?;
                if retryable {
                    summary.retried += 1;
                } else {
                    summary.paused += 1;
                    summary.authentication_failed |= authentication_failed;
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
    persist(cache_root, &store)?;
    notify_scheduler();
    Ok(())
}

pub fn due_account_ids(cache_root: &Path) -> Result<Vec<String>> {
    let _outbox_guard = outbox_guard();
    let now = now_seconds();
    let mut seen = HashSet::new();
    Ok(load(cache_root)?
        .messages
        .into_iter()
        .filter(|message| {
            !message.paused
                && message.next_attempt_at <= now
                && seen.insert(message.account_id.clone())
        })
        .map(|message| message.account_id)
        .collect())
}

pub fn next_due_delay(cache_root: &Path) -> Result<Option<Duration>> {
    let _outbox_guard = outbox_guard();
    let now = now_seconds();
    Ok(load(cache_root)?
        .messages
        .iter()
        .filter(|message| !message.paused)
        .map(|message| message.next_attempt_at.saturating_sub(now))
        .min()
        .map(Duration::from_secs))
}

pub fn scheduler_generation() -> u64 {
    *SCHEDULER_WAKE
        .get_or_init(|| (Mutex::new(0), Condvar::new()))
        .0
        .lock()
        .expect("MailGo outbox scheduler lock poisoned")
}

pub fn wait_for_scheduler_change(observed: u64, timeout: Duration) {
    let (generation, wake) = SCHEDULER_WAKE.get_or_init(|| (Mutex::new(0), Condvar::new()));
    let generation = generation
        .lock()
        .expect("MailGo outbox scheduler lock poisoned");
    if *generation != observed {
        return;
    }
    drop(
        wake.wait_timeout_while(generation, timeout, |current| *current == observed)
            .expect("MailGo outbox scheduler wait poisoned"),
    );
}

pub fn notify_scheduler() {
    let (generation, wake) = SCHEDULER_WAKE.get_or_init(|| (Mutex::new(0), Condvar::new()));
    let mut generation = generation
        .lock()
        .expect("MailGo outbox scheduler lock poisoned");
    *generation = generation.wrapping_add(1);
    wake.notify_one();
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
    let mut store: OutboxStore = serde_json::from_slice(&decoded).context("parse outbox")?;
    validate_store(&store)?;
    store.schema_version = STORE_SCHEMA_VERSION;
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
    if let Some(draft_id) = &message.draft_id {
        validate_message_id(draft_id)?;
    }
    validate_text(&message.to, MAX_RECIPIENT_BYTES, "to", false)?;
    validate_text(&message.cc, MAX_RECIPIENT_BYTES, "cc", true)?;
    validate_text(&message.bcc, MAX_RECIPIENT_BYTES, "bcc", true)?;
    validate_text(&message.subject, MAX_SUBJECT_BYTES, "subject", false)?;
    validate_text(&message.text_body, MAX_BODY_BYTES, "text body", true)?;
    if let Some(html) = &message.html_body {
        validate_text(html, MAX_BODY_BYTES, "HTML body", true)?;
    }
    crate::send::validate_thread_headers(message.in_reply_to.as_deref(), &message.references)?;
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
    if let Some(scheduled_at) = message.scheduled_at {
        if scheduled_at < message.created_at
            || scheduled_at
                > message
                    .created_at
                    .saturating_add(MAX_SCHEDULE_AHEAD_SECONDS)
        {
            return Err(anyhow!(
                "outbox scheduled send time is outside the safe range"
            ));
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
            draft_id: Some("draft-fixture".into()),
            to: "person@example.com".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Queued message".into(),
            text_body: "body".into(),
            html_body: Some("<p>body</p>".into()),
            in_reply_to: Some("parent@example.com".into()),
            references: vec!["root@example.com".into(), "parent@example.com".into()],
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
            scheduled_at: None,
            paused: false,
            last_error: None,
        }
    }

    fn draft_fixture(body: &str) -> crate::drafts::Draft {
        crate::drafts::Draft {
            id: "draft-fixture".into(),
            account_id: "account-1".into(),
            to: "person@example.com".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Queued message".into(),
            body: body.into(),
            html_mode: false,
            html_body: None,
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            updated_at: 0,
        }
    }

    #[test]
    fn encrypted_outbox_round_trips_and_filters_by_account() {
        let root = std::env::temp_dir().join(format!("mailgo-outbox-test-{}", std::process::id()));
        let saved = enqueue(&root, fixture()).expect("enqueue message");
        assert_eq!(saved.in_reply_to.as_deref(), Some("parent@example.com"));
        assert_eq!(saved.references.len(), 2);
        assert_eq!(status(&root, "account-1").unwrap().total, 1);
        assert_eq!(status(&root, "account-2").unwrap().total, 0);
        assert!(remove(&root, "account-1", &saved.id).unwrap());
        assert_eq!(status(&root, "account-1").unwrap().total, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn snapshot_is_bounded_and_never_serializes_attachment_bytes() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-snapshot-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut message = fixture();
        message.text_body = format!("  {}\n{}  ", "摘要 ".repeat(180), "结尾");
        enqueue(&root, message).expect("enqueue snapshot fixture");
        let snapshot = snapshot(&root, "account-1").expect("read snapshot");
        assert_eq!(snapshot.status.total, 1);
        assert_eq!(snapshot.items.len(), 1);
        assert!(snapshot.items[0].preview.chars().count() <= MAX_PREVIEW_CHARS);
        assert_eq!(snapshot.items[0].attachments[0].size, 5);
        let json = serde_json::to_value(&snapshot).expect("serialize snapshot");
        assert!(json["items"][0]["attachments"][0].get("bytes").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recall_reconstructs_a_missing_draft_and_its_attachments() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-recall-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let saved = enqueue(&root, fixture()).expect("enqueue recall fixture");
        let result = recall_to_draft(&root, "account-1", &saved.id).expect("recall message");
        assert_eq!(result.status, RecallOutboxStatus::Recalled);
        let draft = result.draft.expect("reconstructed draft");
        assert_eq!(draft.id, "draft-fixture");
        assert_eq!(draft.html_body.as_deref(), Some("<p>body</p>"));
        assert_eq!(draft.attachments.len(), 1);
        let attachment =
            crate::drafts::load_attachment(&root, "account-1", &draft.id, &draft.attachments[0].id)
                .expect("load reconstructed attachment");
        assert_eq!(attachment.bytes, b"hello");
        assert_eq!(status(&root, "account-1").unwrap().total, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recall_preserves_an_existing_editable_draft() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-existing-draft-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        crate::drafts::save(&root, draft_fixture("newer local edit")).expect("save linked draft");
        let saved = enqueue(&root, fixture()).expect("enqueue existing-draft fixture");
        let result = recall_to_draft(&root, "account-1", &saved.id).expect("recall message");
        assert_eq!(result.status, RecallOutboxStatus::Recalled);
        assert_eq!(
            result.draft.expect("existing draft").body,
            "newer local edit"
        );
        assert_eq!(status(&root, "account-1").unwrap().total, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discard_removes_the_linked_draft_after_queue_persistence() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-discard-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        crate::drafts::save(&root, draft_fixture("discard me")).expect("save linked draft");
        let saved = enqueue(&root, fixture()).expect("enqueue discard fixture");
        assert_eq!(
            discard_queued(&root, "account-1", &saved.id).unwrap(),
            DiscardOutboxStatus::Discarded
        );
        assert!(crate::drafts::list(&root, "account-1").unwrap().is_empty());
        assert_eq!(status(&root, "account-1").unwrap().total, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_actions_refuse_messages_already_being_sent() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-in-flight-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut message = fixture();
        message.id = format!("outbox-in-flight-{:016x}", rand::random::<u64>());
        let saved = enqueue(&root, message).expect("enqueue in-flight fixture");
        let key = (saved.account_id.clone(), saved.id.clone());
        IN_FLIGHT_IDS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .expect("in-flight lock")
            .insert(key.clone());
        let guard = InFlightGuard { keys: vec![key] };

        assert_eq!(
            retry_queued(&root, &saved.account_id, &saved.id).unwrap(),
            RetryOutboxStatus::TooLate
        );
        assert_eq!(
            discard_queued(&root, &saved.account_id, &saved.id).unwrap(),
            DiscardOutboxStatus::TooLate
        );
        assert_eq!(
            recall_to_draft(&root, &saved.account_id, &saved.id)
                .unwrap()
                .status,
            RecallOutboxStatus::TooLate
        );
        drop(guard);
        assert_eq!(
            discard_queued(&root, &saved.account_id, &saved.id).unwrap(),
            DiscardOutboxStatus::Discarded
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn flush_leases_serialize_one_account_without_blocking_another() {
        let first_account = format!("account-a-{:016x}", rand::random::<u64>());
        let second_account = format!("account-b-{:016x}", rand::random::<u64>());
        let first = FlushAccountGuard::try_acquire(&first_account).expect("first account lease");
        assert!(FlushAccountGuard::try_acquire(&first_account).is_none());
        let second = FlushAccountGuard::try_acquire(&second_account).expect("second account lease");
        drop(first);
        assert!(FlushAccountGuard::try_acquire(&first_account).is_some());
        drop(second);
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
    fn legacy_outbox_messages_default_without_reply_headers() {
        let message: QueuedMessage = serde_json::from_value(serde_json::json!({
            "id": "legacy",
            "accountId": "account-1",
            "to": "person@example.com",
            "cc": "",
            "bcc": "",
            "subject": "Legacy",
            "textBody": "body",
            "htmlBody": null,
            "attachments": [],
            "createdAt": 1,
            "updatedAt": 1,
            "attempts": 0,
            "nextAttemptAt": 1,
            "paused": false,
            "lastError": null
        }))
        .expect("legacy outbox message");
        assert!(message.draft_id.is_none());
        assert!(message.in_reply_to.is_none());
        assert!(message.references.is_empty());
        assert!(message.scheduled_at.is_none());
        validate(&message).expect("legacy outbox message remains valid");
    }

    #[test]
    fn legacy_outbox_store_upgrades_without_inventing_a_schedule() {
        let payload = serde_json::to_vec(&OutboxStore {
            schema_version: 1,
            messages: vec![fixture()],
        })
        .expect("serialize legacy outbox");
        let encrypted = crate::sync::protect_cache(&payload).expect("encrypt legacy outbox");
        let upgraded = decode(&encrypted).expect("decode legacy outbox");
        assert_eq!(upgraded.schema_version, STORE_SCHEMA_VERSION);
        assert!(upgraded.messages[0].scheduled_at.is_none());
    }

    #[test]
    fn retry_all_unpauses_only_paused_messages() {
        let root =
            std::env::temp_dir().join(format!("mailgo-outbox-resume-{}", std::process::id()));
        let mut store = OutboxStore {
            schema_version: STORE_SCHEMA_VERSION,
            messages: vec![
                QueuedMessage {
                    paused: true,
                    last_error: Some("账户需要重新授权后才能发送".into()),
                    ..fixture()
                },
                QueuedMessage {
                    id: "outbox-scheduled".into(),
                    next_attempt_at: now_seconds().saturating_add(30),
                    ..fixture()
                },
            ],
        };
        persist(&root, &store).expect("persist paused fixture");
        assert_eq!(status(&root, "account-1").unwrap().paused, 1);
        assert_eq!(retry_all(&root, "account-1").unwrap(), 1);
        assert_eq!(status(&root, "account-1").unwrap().paused, 0);
        assert_eq!(status(&root, "account-1").unwrap().scheduled, 1);
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

    #[test]
    fn delayed_messages_can_only_be_cancelled_before_they_are_due() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-undo-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let generation = scheduler_generation();
        let scheduled = enqueue_with_delay(&root, fixture(), MAX_UNDO_SEND_SECONDS)
            .expect("enqueue scheduled message");
        assert!(scheduler_generation() != generation);
        let delayed_status = status(&root, "account-1").unwrap();
        assert_eq!(delayed_status.scheduled, 1);
        assert_eq!(delayed_status.user_scheduled, 0);
        assert_eq!(delayed_status.undoable, 1);
        assert!(snapshot(&root, "account-1").unwrap().items[0]
            .scheduled_at
            .is_none());
        let delay = next_due_delay(&root).unwrap().expect("scheduled delay");
        assert!(delay > Duration::ZERO);
        assert!(delay <= Duration::from_secs(MAX_UNDO_SEND_SECONDS));
        assert_eq!(
            cancel_scheduled(&root, "account-1", &scheduled.id).unwrap(),
            CancelScheduledResult::Cancelled
        );
        assert_eq!(status(&root, "account-1").unwrap().total, 0);
        assert_eq!(
            cancel_scheduled(&root, "account-1", &scheduled.id).unwrap(),
            CancelScheduledResult::Missing
        );

        let due = enqueue(&root, fixture()).expect("enqueue immediately due message");
        assert_eq!(
            cancel_scheduled(&root, "account-1", &due.id).unwrap(),
            CancelScheduledResult::TooLate
        );
        assert_eq!(status(&root, "account-1").unwrap().total, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_schedule_stays_distinct_and_can_be_sent_immediately() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-explicit-schedule-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let scheduled_at = now_seconds().saturating_add(MIN_SCHEDULE_LEAD_SECONDS + 5);
        let mut message = fixture();
        message.created_at = 0;
        message.updated_at = 0;
        let saved = enqueue_at(&root, message, scheduled_at).expect("enqueue explicit schedule");
        assert_eq!(saved.scheduled_at, Some(scheduled_at));
        assert_eq!(saved.next_attempt_at, scheduled_at);
        assert!(due_account_ids(&root).unwrap().is_empty());

        let scheduled_status = status(&root, "account-1").unwrap();
        assert_eq!(scheduled_status.scheduled, 1);
        assert_eq!(scheduled_status.user_scheduled, 1);
        assert_eq!(scheduled_status.undoable, 0);
        let scheduled_snapshot = snapshot(&root, "account-1").unwrap();
        assert_eq!(scheduled_snapshot.items[0].scheduled_at, Some(scheduled_at));
        assert_eq!(
            scheduled_snapshot.items[0].state,
            OutboxItemState::Scheduled
        );

        assert_eq!(
            pause_account(&root, "account-1", "authorization required").unwrap(),
            1
        );
        assert_eq!(resume_account(&root, "account-1").unwrap(), 1);
        let resumed = snapshot(&root, "account-1").unwrap();
        assert_eq!(resumed.items[0].next_attempt_at, scheduled_at);
        assert_eq!(resumed.items[0].scheduled_at, Some(scheduled_at));

        assert_eq!(
            retry_queued(&root, "account-1", &saved.id).unwrap(),
            RetryOutboxStatus::Retried
        );
        let immediate = snapshot(&root, "account-1").unwrap();
        assert_eq!(immediate.items[0].state, OutboxItemState::Pending);
        assert!(immediate.items[0].scheduled_at.is_none());
        assert_eq!(due_account_ids(&root).unwrap(), vec!["account-1"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_schedule_rejects_past_and_unbounded_times() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-schedule-bounds-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now = now_seconds();
        assert!(enqueue_at(
            &root,
            fixture(),
            now.saturating_add(MIN_SCHEDULE_LEAD_SECONDS - 1)
        )
        .is_err());
        assert!(enqueue_at(
            &root,
            fixture(),
            now.saturating_add(MAX_SCHEDULE_AHEAD_SECONDS + 60)
        )
        .is_err());
        assert!(!root.exists());
    }

    #[test]
    fn pausing_an_account_removes_it_from_the_due_schedule() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-outbox-pause-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        enqueue(&root, fixture()).expect("enqueue due message");
        assert_eq!(due_account_ids(&root).unwrap(), vec!["account-1"]);
        assert_eq!(
            pause_account(&root, "account-1", "账户需要重新授权后才能发送").unwrap(),
            1
        );
        assert!(due_account_ids(&root).unwrap().is_empty());
        let status = status(&root, "account-1").unwrap();
        assert_eq!(status.paused, 1);
        assert_eq!(status.pending, 0);
        assert!(next_due_delay(&root).unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }
}
