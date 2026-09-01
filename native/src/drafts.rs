use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_FILE: &str = "drafts.bin";
const STORE_BACKUP_FILE: &str = "drafts.bin.bak";
const STORE_TEMP_FILE: &str = "drafts.bin.tmp";
const MAX_DRAFTS: usize = 100;
const MAX_DRAFT_ID_BYTES: usize = 128;
const MAX_RECIPIENT_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

static DRAFTS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn drafts_guard() -> std::sync::MutexGuard<'static, ()> {
    DRAFTS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("MailGo drafts lock poisoned")
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

pub fn save(cache_root: &Path, mut draft: Draft) -> Result<Draft> {
    let _drafts_guard = drafts_guard();
    validate(&draft)?;
    if draft.id.is_empty() {
        draft.id = format!("draft-{:016x}", rand::random::<u64>());
    }
    let mut store = load(cache_root)?;
    if store
        .drafts
        .iter()
        .any(|existing| existing.id == draft.id && existing.account_id != draft.account_id)
    {
        return Err(anyhow!("draft does not belong to this account"));
    }
    store.drafts.retain(|existing| existing.id != draft.id);
    draft.updated_at = now_seconds();
    store.drafts.push(draft.clone());
    store
        .drafts
        .sort_by_key(|existing| std::cmp::Reverse(existing.updated_at));
    store.drafts.truncate(MAX_DRAFTS);
    persist(cache_root, &store)?;
    Ok(draft)
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
    if store.drafts.len() != original_len {
        persist(cache_root, &store)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<()> {
    let _drafts_guard = drafts_guard();
    validate_account_id(account_id)?;
    let mut store = load(cache_root)?;
    let original_len = store.drafts.len();
    store.drafts.retain(|draft| draft.account_id != account_id);
    if store.drafts.len() != original_len {
        persist(cache_root, &store)?;
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
    Ok(())
}

fn validate_draft_id(value: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    if value.len() > MAX_DRAFT_ID_BYTES
        || value == "."
        || value == ".."
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(anyhow!("invalid draft id"));
    }
    Ok(())
}

fn validate_account_id(value: &str) -> Result<()> {
    if !crate::valid_account_id(value) {
        return Err(anyhow!("invalid account id"));
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
    let store: DraftStore = serde_json::from_slice(&decoded).context("parse drafts")?;
    if store.schema_version > STORE_SCHEMA_VERSION || store.drafts.len() > MAX_DRAFTS {
        return Err(anyhow!("unsupported or oversized draft store"));
    }
    for draft in &store.drafts {
        validate(draft)?;
    }
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

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            updated_at: 1,
        }
    }

    #[test]
    fn encrypted_drafts_round_trip_and_filter_by_account() {
        let root = std::env::temp_dir().join(format!("mailgo-drafts-test-{}", std::process::id()));
        let saved = save(&root, fixture()).expect("save draft");
        let drafts = list(&root, "account-1").unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].in_reply_to.as_deref(), Some("parent@example.com"));
        assert_eq!(drafts[0].references.len(), 2);
        assert!(list(&root, "account-2").unwrap().is_empty());
        assert!(remove(&root, "account-1", &saved.id).unwrap());
        assert!(list(&root, "account-1").unwrap().is_empty());
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
    fn legacy_drafts_default_without_reply_headers() {
        let draft: Draft = serde_json::from_str(
            r#"{"id":"legacy","accountId":"account-1","to":"person@example.com","cc":"","bcc":"","subject":"Legacy","body":"body","htmlMode":false,"updatedAt":1}"#,
        )
        .expect("legacy draft");
        assert!(draft.in_reply_to.is_none());
        assert!(draft.references.is_empty());
        validate(&draft).expect("legacy draft remains valid");
    }

    #[test]
    fn cannot_overwrite_another_accounts_draft() {
        let root =
            std::env::temp_dir().join(format!("mailgo-drafts-owner-test-{}", std::process::id()));
        save(&root, fixture()).expect("save owner draft");
        let mut other = fixture();
        other.account_id = "account-2".into();
        assert!(save(&root, other).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_saves_preserve_each_draft() {
        let root = std::env::temp_dir().join(format!(
            "mailgo-drafts-concurrent-test-{}",
            std::process::id()
        ));
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
