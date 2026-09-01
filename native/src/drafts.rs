use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const STORE_SCHEMA_VERSION: u32 = 2;
const STORE_FILE: &str = "drafts.bin";
const STORE_BACKUP_FILE: &str = "drafts.bin.bak";
const STORE_TEMP_FILE: &str = "drafts.bin.tmp";
const ATTACHMENT_ROOT: &str = "draft-attachments-v1";
const MAX_DRAFTS: usize = 100;
const MAX_DRAFT_ID_BYTES: usize = 128;
const MAX_ATTACHMENT_ID_BYTES: usize = 128;
const MAX_RECIPIENT_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_ATTACHMENT_TOTAL_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_ATTACHMENTS: usize = 10;

static DRAFTS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn drafts_guard() -> std::sync::MutexGuard<'static, ()> {
    DRAFTS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("MailGo drafts lock poisoned")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DraftAttachment {
    pub id: String,
    pub file_name: String,
    pub content_type: String,
    #[serde(default)]
    pub content_id: Option<String>,
    pub size: u64,
}

#[derive(Debug)]
pub struct DraftAttachmentData {
    pub metadata: DraftAttachment,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct NewDraftAttachment {
    pub id: String,
    pub file_name: String,
    pub content_type: String,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Draft {
    pub id: String,
    pub account_id: String,
    pub to: String,
    #[serde(default)]
    pub cc: String,
    #[serde(default)]
    pub bcc: String,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub html_mode: bool,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<DraftAttachment>,
    pub updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftStore {
    schema_version: u32,
    drafts: Vec<Draft>,
}

pub fn list(cache_root: &Path, account_id: &str) -> Result<Vec<Draft>> {
    let _drafts_guard = drafts_guard();
    validate_account_id(account_id)?;
    let mut drafts = load(cache_root)?
        .drafts
        .into_iter()
        .filter(|draft| draft.account_id == account_id)
        .collect::<Vec<_>>();
    drafts.sort_by_key(|draft| std::cmp::Reverse(draft.updated_at));
    Ok(drafts)
}

/// Saves draft text while preserving attachment metadata already committed by the native layer.
pub fn save(cache_root: &Path, mut draft: Draft) -> Result<Draft> {
    let _drafts_guard = drafts_guard();
    validate(&draft)?;
    if draft.id.is_empty() {
        draft.id = format!("draft-{:016x}", rand::random::<u64>());
    }
    let mut store = load(cache_root)?;
    let existing = store
        .drafts
        .iter()
        .find(|existing| existing.id == draft.id)
        .cloned();
    if existing
        .as_ref()
        .is_some_and(|existing| existing.account_id != draft.account_id)
    {
        return Err(anyhow!("draft does not belong to this account"));
    }
    if let Some(existing) = existing {
        draft.attachments = existing.attachments;
    } else if !draft.attachments.is_empty() {
        return Err(anyhow!(
            "new draft attachments must be committed separately"
        ));
    }
    validate(&draft)?;
    store.drafts.retain(|existing| existing.id != draft.id);
    draft.updated_at = now_seconds();
    store.drafts.push(draft.clone());
    store
        .drafts
        .sort_by_key(|existing| std::cmp::Reverse(existing.updated_at));
    let evicted = if store.drafts.len() > MAX_DRAFTS {
        store.drafts.split_off(MAX_DRAFTS)
    } else {
        Vec::new()
    };
    persist(cache_root, &store)?;
    for removed in evicted {
        remove_draft_files(cache_root, &removed.account_id, &removed.id);
    }
    Ok(draft)
}

pub fn attach(
    cache_root: &Path,
    account_id: &str,
    draft_id: &str,
    attachment: NewDraftAttachment,
    bytes: &[u8],
) -> Result<Draft> {
    let NewDraftAttachment {
        id: attachment_id,
        file_name,
        content_type,
        content_id,
    } = attachment;
    let _drafts_guard = drafts_guard();
    validate_account_id(account_id)?;
    validate_draft_id(draft_id)?;
    validate_attachment_id(&attachment_id)?;
    validate_file_name(&file_name)?;
    validate_content_type(&content_type)?;
    validate_content_id(content_id.as_deref())?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(anyhow!("draft attachment exceeds the per-file size limit"));
    }

    let mut store = load(cache_root)?;
    let draft = store
        .drafts
        .iter_mut()
        .find(|draft| draft.id == draft_id && draft.account_id == account_id)
        .ok_or_else(|| anyhow!("draft attachment owner is missing"))?;
    if let Some(existing) = draft
        .attachments
        .iter()
        .find(|attachment| attachment.id == attachment_id)
    {
        let matches = existing.file_name == file_name
            && existing.content_type == content_type
            && existing.content_id == content_id
            && existing.size == bytes.len() as u64
            && attachment_path(cache_root, account_id, draft_id, &attachment_id).is_file();
        return if matches {
            Ok(draft.clone())
        } else {
            Err(anyhow!("draft attachment id is already in use"))
        };
    }
    if draft.attachments.len() >= MAX_ATTACHMENTS {
        return Err(anyhow!("draft contains too many attachments"));
    }
    let total = draft
        .attachments
        .iter()
        .try_fold(bytes.len(), |total, attachment| {
            usize::try_from(attachment.size)
                .ok()
                .and_then(|size| total.checked_add(size))
                .ok_or_else(|| anyhow!("draft attachment size overflow"))
        })?;
    if total > MAX_ATTACHMENT_TOTAL_BYTES {
        return Err(anyhow!("draft attachments exceed the total size limit"));
    }

    let attachment = DraftAttachment {
        id: attachment_id,
        file_name,
        content_type,
        content_id,
        size: u64::try_from(bytes.len()).context("draft attachment size overflow")?,
    };
    validate_attachment(&attachment)?;
    write_attachment_file(cache_root, account_id, draft_id, &attachment.id, bytes)?;
    draft.attachments.push(attachment.clone());
    draft.updated_at = now_seconds();
    let saved = draft.clone();
    if let Err(error) = persist(cache_root, &store) {
        remove_attachment_file(cache_root, account_id, draft_id, &attachment.id);
        return Err(error);
    }
    Ok(saved)
}

pub fn remove_attachment(
    cache_root: &Path,
    account_id: &str,
    draft_id: &str,
    attachment_id: &str,
) -> Result<Draft> {
    let _drafts_guard = drafts_guard();
    validate_account_id(account_id)?;
    validate_draft_id(draft_id)?;
    validate_attachment_id(attachment_id)?;
    let mut store = load(cache_root)?;
    let draft = store
        .drafts
        .iter_mut()
        .find(|draft| draft.id == draft_id && draft.account_id == account_id)
        .ok_or_else(|| anyhow!("draft attachment owner is missing"))?;
    let original_len = draft.attachments.len();
    draft
        .attachments
        .retain(|attachment| attachment.id != attachment_id);
    if draft.attachments.len() == original_len {
        return Err(anyhow!("draft attachment is missing"));
    }
    draft.updated_at = now_seconds();
    let saved = draft.clone();
    persist(cache_root, &store)?;
    remove_attachment_file(cache_root, account_id, draft_id, attachment_id);
    Ok(saved)
}

pub fn load_attachment(
    cache_root: &Path,
    account_id: &str,
    draft_id: &str,
    attachment_id: &str,
) -> Result<DraftAttachmentData> {
    validate_account_id(account_id)?;
    validate_draft_id(draft_id)?;
    validate_attachment_id(attachment_id)?;
    let metadata = {
        let _drafts_guard = drafts_guard();
        let store = load(cache_root)?;
        let draft = store
            .drafts
            .iter()
            .find(|draft| draft.id == draft_id && draft.account_id == account_id)
            .ok_or_else(|| anyhow!("draft attachment owner is missing"))?;
        draft
            .attachments
            .iter()
            .find(|attachment| attachment.id == attachment_id)
            .cloned()
            .ok_or_else(|| anyhow!("draft attachment is missing"))?
    };
    let path = attachment_path(cache_root, account_id, draft_id, attachment_id);
    let encrypted_size = fs::metadata(&path)
        .with_context(|| format!("inspect {}", path.display()))?
        .len();
    if encrypted_size > (MAX_ATTACHMENT_BYTES as u64).saturating_add(64 * 1024) {
        return Err(anyhow!("encrypted draft attachment is oversized"));
    }
    let encrypted = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let bytes = crate::sync::unprotect_cache(&encrypted).context("decrypt draft attachment")?;
    if bytes.len() > MAX_ATTACHMENT_BYTES || u64::try_from(bytes.len()).ok() != Some(metadata.size)
    {
        return Err(anyhow!("draft attachment size does not match its metadata"));
    }
    Ok(DraftAttachmentData { metadata, bytes })
}

pub fn remove(cache_root: &Path, account_id: &str, draft_id: &str) -> Result<bool> {
    let _drafts_guard = drafts_guard();
    validate_account_id(account_id)?;
    validate_draft_id(draft_id)?;
    let mut store = load(cache_root)?;
    let original_len = store.drafts.len();
    store
        .drafts
        .retain(|draft| !(draft.account_id == account_id && draft.id == draft_id));
    let removed = store.drafts.len() != original_len;
    if removed {
        persist(cache_root, &store)?;
    }
    remove_draft_files(cache_root, account_id, draft_id);
    Ok(removed)
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<()> {
    let _drafts_guard = drafts_guard();
    validate_account_id(account_id)?;
    let mut store = load(cache_root)?;
    let removed = store
        .drafts
        .iter()
        .filter(|draft| draft.account_id == account_id)
        .map(|draft| draft.id.clone())
        .collect::<Vec<_>>();
    store.drafts.retain(|draft| draft.account_id != account_id);
    if !removed.is_empty() {
        persist(cache_root, &store)?;
    }
    for draft_id in removed {
        remove_draft_files(cache_root, account_id, &draft_id);
    }
    let account_root = account_attachment_root(cache_root, account_id);
    if account_root.starts_with(cache_root.join(ATTACHMENT_ROOT)) {
        let _ = fs::remove_dir_all(account_root);
    }
    Ok(())
}

fn validate(draft: &Draft) -> Result<()> {
    validate_account_id(&draft.account_id)?;
    validate_draft_id(&draft.id)?;
    validate_text(&draft.to, MAX_RECIPIENT_BYTES, "to")?;
    validate_text(&draft.cc, MAX_RECIPIENT_BYTES, "cc")?;
    validate_text(&draft.bcc, MAX_RECIPIENT_BYTES, "bcc")?;
    validate_text(&draft.subject, MAX_SUBJECT_BYTES, "subject")?;
    validate_text(&draft.body, MAX_BODY_BYTES, "body")?;
    crate::send::validate_thread_headers(draft.in_reply_to.as_deref(), &draft.references)?;
    if draft.attachments.len() > MAX_ATTACHMENTS {
        return Err(anyhow!("draft contains too many attachments"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut total = 0usize;
    for attachment in &draft.attachments {
        validate_attachment(attachment)?;
        if !seen.insert(&attachment.id) {
            return Err(anyhow!("draft contains duplicate attachment ids"));
        }
        total = total
            .checked_add(
                usize::try_from(attachment.size).context("draft attachment size overflow")?,
            )
            .ok_or_else(|| anyhow!("draft attachment size overflow"))?;
    }
    if total > MAX_ATTACHMENT_TOTAL_BYTES {
        return Err(anyhow!("draft attachments exceed the total size limit"));
    }
    Ok(())
}

fn validate_attachment(attachment: &DraftAttachment) -> Result<()> {
    validate_attachment_id(&attachment.id)?;
    validate_file_name(&attachment.file_name)?;
    validate_content_type(&attachment.content_type)?;
    validate_content_id(attachment.content_id.as_deref())?;
    if attachment.size > MAX_ATTACHMENT_BYTES as u64 {
        return Err(anyhow!("draft attachment exceeds the per-file size limit"));
    }
    Ok(())
}

fn validate_draft_id(value: &str) -> Result<()> {
    validate_safe_id(value, MAX_DRAFT_ID_BYTES, true, "draft")
}

fn validate_attachment_id(value: &str) -> Result<()> {
    validate_safe_id(value, MAX_ATTACHMENT_ID_BYTES, false, "attachment")
}

fn validate_safe_id(value: &str, max_bytes: usize, allow_empty: bool, label: &str) -> Result<()> {
    if allow_empty && value.is_empty() {
        return Ok(());
    }
    if value.is_empty()
        || value.len() > max_bytes
        || value == "."
        || value == ".."
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(anyhow!("invalid {label} id"));
    }
    Ok(())
}

fn validate_account_id(value: &str) -> Result<()> {
    if !crate::valid_account_id(value) {
        return Err(anyhow!("invalid account id"));
    }
    Ok(())
}

fn validate_file_name(value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > 255
        || value == "."
        || value == ".."
        || value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n' | '/' | '\\'))
    {
        return Err(anyhow!("invalid draft attachment file name"));
    }
    Ok(())
}

fn validate_content_type(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
    {
        return Err(anyhow!("invalid draft attachment content type"));
    }
    Ok(())
}

fn validate_content_id(value: Option<&str>) -> Result<()> {
    if value.is_some_and(|content_id| {
        content_id.is_empty()
            || content_id.len() > 128
            || !content_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '@')
            })
    }) {
        return Err(anyhow!("invalid draft attachment content id"));
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize, field: &str) -> Result<()> {
    if value.len() > max_bytes
        || value.chars().any(|character| {
            character == '\0' || (field != "body" && matches!(character, '\r' | '\n'))
        })
    {
        return Err(anyhow!(
            "draft field {field} exceeds the safe size or character limit"
        ));
    }
    Ok(())
}

fn load(cache_root: &Path) -> Result<DraftStore> {
    let path = cache_root.join(STORE_FILE);
    let backup = cache_root.join(STORE_BACKUP_FILE);
    let primary = fs::read(&path);
    let store = match primary {
        Ok(bytes) => decode(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(anyhow!("missing")),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    };
    let store = match store {
        Ok(store) => store,
        Err(primary_error) => match fs::read(&backup) {
            Ok(bytes) => decode(&bytes)
                .with_context(|| format!("parse {} after {}", backup.display(), primary_error))?,
            Err(backup_error) if backup_error.kind() == std::io::ErrorKind::NotFound => {
                if primary_error.to_string() == "missing" {
                    return Ok(DraftStore {
                        schema_version: STORE_SCHEMA_VERSION,
                        drafts: Vec::new(),
                    });
                }
                return Err(primary_error);
            }
            Err(backup_error) => {
                return Err(backup_error).with_context(|| format!("read {}", backup.display()))
            }
        },
    };
    Ok(store)
}

fn decode(bytes: &[u8]) -> Result<DraftStore> {
    let decoded = crate::sync::unprotect_cache(bytes).context("decrypt drafts")?;
    let mut store: DraftStore = serde_json::from_slice(&decoded).context("parse drafts")?;
    if store.schema_version > STORE_SCHEMA_VERSION || store.drafts.len() > MAX_DRAFTS {
        return Err(anyhow!("unsupported or oversized draft store"));
    }
    for draft in &store.drafts {
        validate(draft)?;
    }
    store.schema_version = STORE_SCHEMA_VERSION;
    Ok(store)
}

fn persist(cache_root: &Path, store: &DraftStore) -> Result<()> {
    fs::create_dir_all(cache_root).with_context(|| format!("create {}", cache_root.display()))?;
    let payload = serde_json::to_vec(store).context("serialize drafts")?;
    let encrypted = crate::sync::protect_cache(&payload).context("encrypt drafts")?;
    let temporary = cache_root.join(STORE_TEMP_FILE);
    let path = cache_root.join(STORE_FILE);
    let backup = cache_root.join(STORE_BACKUP_FILE);
    fs::write(&temporary, encrypted).context("write drafts")?;
    if path.exists() {
        let _ = fs::remove_file(&backup);
        fs::rename(&path, &backup).context("backup drafts")?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::rename(&backup, &path);
        return Err(error).context("commit drafts");
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn account_attachment_root(cache_root: &Path, account_id: &str) -> PathBuf {
    let digest = Sha256::digest(account_id.as_bytes());
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut scope = String::with_capacity(digest.len() * 2);
    for byte in digest {
        scope.push(char::from(HEX[usize::from(byte >> 4)]));
        scope.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    cache_root.join(ATTACHMENT_ROOT).join(scope)
}

fn draft_attachment_root(cache_root: &Path, account_id: &str, draft_id: &str) -> PathBuf {
    account_attachment_root(cache_root, account_id).join(draft_id)
}

fn attachment_path(
    cache_root: &Path,
    account_id: &str,
    draft_id: &str,
    attachment_id: &str,
) -> PathBuf {
    draft_attachment_root(cache_root, account_id, draft_id).join(format!("{attachment_id}.bin"))
}

fn write_attachment_file(
    cache_root: &Path,
    account_id: &str,
    draft_id: &str,
    attachment_id: &str,
    bytes: &[u8],
) -> Result<()> {
    let directory = draft_attachment_root(cache_root, account_id, draft_id);
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let encrypted = crate::sync::protect_cache(bytes).context("encrypt draft attachment")?;
    let temporary = directory.join(format!(
        "{attachment_id}.tmp-{:016x}",
        rand::random::<u64>()
    ));
    let path = attachment_path(cache_root, account_id, draft_id, attachment_id);
    fs::write(&temporary, encrypted).with_context(|| format!("write {}", temporary.display()))?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("commit {}", path.display()));
    }
    Ok(())
}

fn remove_attachment_file(
    cache_root: &Path,
    account_id: &str,
    draft_id: &str,
    attachment_id: &str,
) {
    let _ = fs::remove_file(attachment_path(
        cache_root,
        account_id,
        draft_id,
        attachment_id,
    ));
    let _ = fs::remove_dir(draft_attachment_root(cache_root, account_id, draft_id));
}

fn remove_draft_files(cache_root: &Path, account_id: &str, draft_id: &str) {
    let directory = draft_attachment_root(cache_root, account_id, draft_id);
    if directory.starts_with(cache_root.join(ATTACHMENT_ROOT)) {
        let _ = fs::remove_dir_all(directory);
    }
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

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mailgo-drafts-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn fixture() -> Draft {
        Draft {
            id: "draft-fixture".into(),
            account_id: "account-1".into(),
            to: "person@example.com".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Draft subject".into(),
            body: "Draft body".into(),
            html_mode: false,
            in_reply_to: Some("parent@example.com".into()),
            references: vec!["root@example.com".into(), "parent@example.com".into()],
            attachments: Vec::new(),
            updated_at: 1,
        }
    }

    fn attachment_fixture(id: &str, size: u64) -> DraftAttachment {
        DraftAttachment {
            id: id.into(),
            file_name: format!("{id}.bin"),
            content_type: "application/octet-stream".into(),
            content_id: None,
            size,
        }
    }

    #[test]
    fn encrypted_drafts_round_trip_and_filter_by_account() {
        let root = test_root("roundtrip");
        let saved = save(&root, fixture()).expect("save draft");
        let drafts = list(&root, "account-1").unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].in_reply_to.as_deref(), Some("parent@example.com"));
        assert_eq!(drafts[0].references.len(), 2);
        assert!(drafts[0].attachments.is_empty());
        assert!(list(&root, "account-2").unwrap().is_empty());
        assert!(remove(&root, "account-1", &saved.id).unwrap());
        assert!(list(&root, "account-1").unwrap().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn encrypted_attachment_survives_text_saves_and_contains_no_plaintext() {
        let root = test_root("attachment-roundtrip");
        let draft = save(&root, fixture()).unwrap();
        let payload = b"secret draft attachment payload";
        let draft = attach(
            &root,
            "account-1",
            &draft.id,
            NewDraftAttachment {
                id: "attachment-private".into(),
                file_name: "private-note.txt".into(),
                content_type: "text/plain".into(),
                content_id: None,
            },
            payload,
        )
        .unwrap();
        assert_eq!(draft.attachments.len(), 1);
        let retried = attach(
            &root,
            "account-1",
            &draft.id,
            NewDraftAttachment {
                id: "attachment-private".into(),
                file_name: "private-note.txt".into(),
                content_type: "text/plain".into(),
                content_id: None,
            },
            payload,
        )
        .unwrap();
        assert_eq!(retried.attachments.len(), 1);
        let attachment = draft.attachments[0].clone();
        let encrypted = fs::read(attachment_path(
            &root,
            "account-1",
            &draft.id,
            &attachment.id,
        ))
        .unwrap();
        assert!(!encrypted
            .windows(payload.len())
            .any(|window| window == payload));
        assert!(!encrypted
            .windows("private-note.txt".len())
            .any(|window| window == b"private-note.txt"));
        let encrypted_metadata = fs::read(root.join(STORE_FILE)).unwrap();
        assert!(!encrypted_metadata
            .windows("private-note.txt".len())
            .any(|window| window == b"private-note.txt"));
        assert!(!encrypted_metadata
            .windows(payload.len())
            .any(|window| window == payload));

        let mut edited = fixture();
        edited.subject = "Edited without rewriting attachments".into();
        let edited = save(&root, edited).unwrap();
        assert_eq!(edited.attachments, vec![attachment.clone()]);
        let loaded = load_attachment(&root, "account-1", &draft.id, &attachment.id).unwrap();
        assert_eq!(loaded.bytes, payload);
        assert_eq!(loaded.metadata, attachment);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn attachment_ownership_limits_and_removal_are_enforced() {
        let root = test_root("attachment-security");
        let draft = save(&root, fixture()).unwrap();
        let draft = attach(
            &root,
            "account-1",
            &draft.id,
            NewDraftAttachment {
                id: "attachment-safe".into(),
                file_name: "safe.bin".into(),
                content_type: "application/octet-stream".into(),
                content_id: None,
            },
            b"bytes",
        )
        .unwrap();
        let attachment_id = draft.attachments[0].id.clone();
        assert!(load_attachment(&root, "account-2", &draft.id, &attachment_id).is_err());
        assert!(attach(
            &root,
            "account-1",
            &draft.id,
            NewDraftAttachment {
                id: "attachment-oversized".into(),
                file_name: "oversized.bin".into(),
                content_type: "application/octet-stream".into(),
                content_id: None,
            },
            &vec![0u8; MAX_ATTACHMENT_BYTES + 1],
        )
        .is_err());
        let updated = remove_attachment(&root, "account-1", &draft.id, &attachment_id).unwrap();
        assert!(updated.attachments.is_empty());
        assert!(load_attachment(&root, "account-1", &draft.id, &attachment_id).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn attachment_metadata_count_and_total_limits_are_enforced() {
        let mut too_many = fixture();
        too_many.attachments = (0..=MAX_ATTACHMENTS)
            .map(|index| attachment_fixture(&format!("attachment-{index}"), 1))
            .collect();
        assert!(validate(&too_many).is_err());

        let mut too_large = fixture();
        too_large.attachments = vec![
            attachment_fixture("first", MAX_ATTACHMENT_BYTES as u64),
            attachment_fixture("second", MAX_ATTACHMENT_BYTES as u64),
            attachment_fixture("third", 1),
        ];
        assert!(validate(&too_large).is_err());
    }

    #[test]
    fn corrupt_attachment_does_not_break_draft_listing() {
        let root = test_root("corrupt-attachment");
        let draft = save(&root, fixture()).unwrap();
        let draft = attach(
            &root,
            "account-1",
            &draft.id,
            NewDraftAttachment {
                id: "attachment-corrupt".into(),
                file_name: "corrupt.bin".into(),
                content_type: "application/octet-stream".into(),
                content_id: None,
            },
            b"healthy",
        )
        .unwrap();
        let attachment = draft.attachments[0].clone();
        fs::write(
            attachment_path(&root, "account-1", &draft.id, &attachment.id),
            b"not encrypted",
        )
        .unwrap();
        assert_eq!(list(&root, "account-1").unwrap()[0].attachments.len(), 1);
        assert!(load_attachment(&root, "account-1", &draft.id, &attachment.id).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_attachment_commits_preserve_every_file() {
        let root = test_root("concurrent-attachments");
        let draft = save(&root, fixture()).unwrap();
        let workers = (0..8)
            .map(|index| {
                let root = root.clone();
                let draft_id = draft.id.clone();
                std::thread::spawn(move || {
                    attach(
                        &root,
                        "account-1",
                        &draft_id,
                        NewDraftAttachment {
                            id: format!("attachment-{index}"),
                            file_name: format!("file-{index}.bin"),
                            content_type: "application/octet-stream".into(),
                            content_id: None,
                        },
                        &[index as u8],
                    )
                    .expect("concurrent attachment commit");
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().expect("attachment worker should finish");
        }
        let saved = list(&root, "account-1").unwrap().remove(0);
        assert_eq!(saved.attachments.len(), 8);
        for attachment in saved.attachments {
            assert_eq!(
                load_attachment(&root, "account-1", &saved.id, &attachment.id)
                    .unwrap()
                    .bytes
                    .len(),
                1
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn draft_eviction_cleans_encrypted_attachment_directory() {
        let root = test_root("eviction-cleanup");
        let attachment = attachment_fixture("old-attachment", 3);
        let mut drafts = (0..MAX_DRAFTS)
            .map(|index| {
                let mut draft = fixture();
                draft.id = format!("draft-{index}");
                draft.updated_at = (index + 1) as u64;
                draft
            })
            .collect::<Vec<_>>();
        drafts[0].attachments.push(attachment.clone());
        write_attachment_file(&root, "account-1", &drafts[0].id, &attachment.id, b"old").unwrap();
        let old_path = attachment_path(&root, "account-1", &drafts[0].id, &attachment.id);
        persist(
            &root,
            &DraftStore {
                schema_version: STORE_SCHEMA_VERSION,
                drafts,
            },
        )
        .unwrap();
        let mut newest = fixture();
        newest.id = "newest-draft".into();
        save(&root, newest).unwrap();
        assert_eq!(list(&root, "account-1").unwrap().len(), MAX_DRAFTS);
        assert!(!old_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removing_draft_and_account_cleans_attachment_files() {
        let root = test_root("cleanup");
        let first = save(&root, fixture()).unwrap();
        let first = attach(
            &root,
            "account-1",
            &first.id,
            NewDraftAttachment {
                id: "attachment-first".into(),
                file_name: "first.bin".into(),
                content_type: "application/octet-stream".into(),
                content_id: None,
            },
            b"first",
        )
        .unwrap();
        let first_path = attachment_path(&root, "account-1", &first.id, &first.attachments[0].id);
        assert!(first_path.exists());
        remove(&root, "account-1", &first.id).unwrap();
        assert!(!first_path.exists());

        let mut second_fixture = fixture();
        second_fixture.id = "second-draft".into();
        let second = save(&root, second_fixture).unwrap();
        let second = attach(
            &root,
            "account-1",
            &second.id,
            NewDraftAttachment {
                id: "attachment-second".into(),
                file_name: "second.bin".into(),
                content_type: "application/octet-stream".into(),
                content_id: None,
            },
            b"second",
        )
        .unwrap();
        let second_path =
            attachment_path(&root, "account-1", &second.id, &second.attachments[0].id);
        remove_account(&root, "account-1").unwrap();
        assert!(!second_path.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_unsafe_or_oversized_drafts() {
        let mut draft = fixture();
        draft.id = "..".into();
        assert!(save(std::path::Path::new("."), draft).is_err());

        let mut draft = fixture();
        draft.subject = "unsafe\r\nX-Injected: value".into();
        assert!(save(std::path::Path::new("."), draft).is_err());
    }

    #[test]
    fn legacy_drafts_default_without_reply_headers_or_attachments() {
        let draft: Draft = serde_json::from_str(
            r#"{"id":"legacy","accountId":"account-1","to":"person@example.com","cc":"","bcc":"","subject":"Legacy","body":"body","htmlMode":false,"updatedAt":1}"#,
        )
        .expect("legacy draft");
        assert!(draft.in_reply_to.is_none());
        assert!(draft.references.is_empty());
        assert!(draft.attachments.is_empty());
        validate(&draft).expect("legacy draft remains valid");

        let payload = serde_json::json!({
            "schemaVersion": 1,
            "drafts": [serde_json::from_str::<serde_json::Value>(
                r#"{"id":"legacy","accountId":"account-1","to":"person@example.com","cc":"","bcc":"","subject":"Legacy","body":"body","htmlMode":false,"updatedAt":1}"#,
            ).unwrap()],
        });
        let protected = crate::sync::protect_cache(&serde_json::to_vec(&payload).unwrap()).unwrap();
        let upgraded = decode(&protected).unwrap();
        assert_eq!(upgraded.schema_version, STORE_SCHEMA_VERSION);
        assert!(upgraded.drafts[0].attachments.is_empty());
    }

    #[test]
    fn cannot_overwrite_another_accounts_draft() {
        let root = test_root("owner");
        save(&root, fixture()).expect("save owner draft");
        let mut other = fixture();
        other.account_id = "account-2".into();
        assert!(save(&root, other).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_saves_preserve_each_draft() {
        let root = test_root("concurrent");
        let workers = (0..12)
            .map(|index| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut draft = fixture();
                    draft.id = format!("draft-{index}");
                    draft.subject = format!("Subject {index}");
                    save(&root, draft).expect("concurrent draft save");
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().expect("draft worker should finish");
        }
        assert_eq!(list(&root, "account-1").unwrap().len(), 12);
        let _ = fs::remove_dir_all(root);
    }
}
