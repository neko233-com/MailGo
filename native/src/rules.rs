use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::mail::CachedMessage;

const STORE_SCHEMA_VERSION: u32 = 1;
const STORE_FILE: &str = "mail-rules-v1.bin";
const STORE_BACKUP_FILE: &str = "mail-rules-v1.bin.bak";
const STORE_TEMP_FILE: &str = "mail-rules-v1.bin.tmp";
const MAX_RULES: usize = 256;
const MAX_STORE_BYTES: usize = 1024 * 1024;
const MAX_RULE_ID_BYTES: usize = 128;
const MAX_RULE_VALUE_BYTES: usize = 320;

static RULE_CACHE: OnceLock<Mutex<HashMap<PathBuf, RuleStore>>> = OnceLock::new();

fn rule_cache() -> &'static Mutex<HashMap<PathBuf, RuleStore>> {
    RULE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MailRuleKind {
    Sender,
    Domain,
}

impl MailRuleKind {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "sender" => Ok(Self::Sender),
            "domain" => Ok(Self::Domain),
            _ => Err(anyhow!("unsupported mail rule kind")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailRule {
    pub id: String,
    pub account_id: Option<String>,
    pub kind: MailRuleKind,
    pub value: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleStore {
    schema_version: u32,
    rules: Vec<MailRule>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSnapshot {
    pub rules: Vec<MailRule>,
}

pub fn empty_snapshot() -> RuleSnapshot {
    RuleSnapshot { rules: Vec::new() }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn empty_store() -> RuleStore {
    RuleStore {
        schema_version: STORE_SCHEMA_VERSION,
        rules: Vec::new(),
    }
}

fn valid_rule_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RULE_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn normalize_domain(value: &str) -> Result<String> {
    let normalized = value
        .trim()
        .trim_start_matches('@')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if normalized.is_empty()
        || normalized.len() > 253
        || !normalized.is_ascii()
        || normalized.chars().any(char::is_whitespace)
    {
        return Err(anyhow!("mail rule domain is invalid"));
    }
    for label in normalized.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(anyhow!("mail rule domain is invalid"));
        }
    }
    Ok(normalized)
}

fn normalize_sender(value: &str) -> Result<String> {
    let normalized = value.trim().to_lowercase();
    if normalized.is_empty()
        || normalized.len() > MAX_RULE_VALUE_BYTES
        || normalized.chars().any(|character| {
            character.is_control() || character.is_whitespace() || matches!(character, '<' | '>')
        })
    {
        return Err(anyhow!("mail rule sender is invalid"));
    }
    let (local, domain) = normalized
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("mail rule sender is invalid"))?;
    if local.is_empty() || local.len() > 64 || local.contains('@') {
        return Err(anyhow!("mail rule sender is invalid"));
    }
    Ok(format!("{local}@{}", normalize_domain(domain)?))
}

pub fn normalize_value(kind: MailRuleKind, value: &str) -> Result<String> {
    match kind {
        MailRuleKind::Sender => normalize_sender(value),
        MailRuleKind::Domain => normalize_domain(value),
    }
}

fn validate_rule(rule: &MailRule) -> Result<()> {
    if !valid_rule_id(&rule.id) {
        return Err(anyhow!("mail rule id is invalid"));
    }
    if let Some(account_id) = rule.account_id.as_deref() {
        if !crate::valid_account_id(account_id) {
            return Err(anyhow!("mail rule account id is invalid"));
        }
    }
    if rule.value.len() > MAX_RULE_VALUE_BYTES
        || normalize_value(rule.kind, &rule.value)? != rule.value
    {
        return Err(anyhow!("mail rule value is not normalized"));
    }
    Ok(())
}

fn validate_store(store: &RuleStore) -> Result<()> {
    if store.schema_version > STORE_SCHEMA_VERSION || store.rules.len() > MAX_RULES {
        return Err(anyhow!("unsupported or oversized mail rule store"));
    }
    let mut ids = HashSet::with_capacity(store.rules.len());
    let mut definitions = HashSet::with_capacity(store.rules.len());
    for rule in &store.rules {
        validate_rule(rule)?;
        if !ids.insert(rule.id.to_ascii_lowercase()) {
            return Err(anyhow!("duplicate mail rule id"));
        }
        if !definitions.insert((
            rule.account_id
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase(),
            rule.kind,
            rule.value.clone(),
        )) {
            return Err(anyhow!("duplicate mail rule"));
        }
    }
    Ok(())
}

fn decode(bytes: &[u8]) -> Result<RuleStore> {
    if bytes.len() > MAX_STORE_BYTES {
        return Err(anyhow!("mail rule store is too large"));
    }
    let decoded = crate::sync::unprotect_cache(bytes).context("decrypt mail rule store")?;
    if decoded.len() > MAX_STORE_BYTES {
        return Err(anyhow!("mail rule store is too large"));
    }
    let mut store: RuleStore = serde_json::from_slice(&decoded).context("parse mail rule store")?;
    validate_store(&store)?;
    store.schema_version = STORE_SCHEMA_VERSION;
    Ok(store)
}

fn load_from_disk(cache_root: &Path) -> Result<RuleStore> {
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
                .with_context(|| format!("recover {} after {primary_error}", backup.display())),
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

fn cached_store(cache_root: &Path, cache: &mut HashMap<PathBuf, RuleStore>) -> Result<RuleStore> {
    if let Some(store) = cache.get(cache_root) {
        return Ok(store.clone());
    }
    let store = load_from_disk(cache_root)?;
    cache.insert(cache_root.to_path_buf(), store.clone());
    Ok(store)
}

fn persist(cache_root: &Path, store: &RuleStore) -> Result<()> {
    validate_store(store)?;
    fs::create_dir_all(cache_root).with_context(|| format!("create {}", cache_root.display()))?;
    let payload = serde_json::to_vec(store).context("serialize mail rule store")?;
    if payload.len() > MAX_STORE_BYTES {
        return Err(anyhow!("mail rule store is too large"));
    }
    let encrypted = crate::sync::protect_cache(&payload).context("encrypt mail rule store")?;
    if encrypted.len() > MAX_STORE_BYTES {
        return Err(anyhow!("mail rule store is too large"));
    }

    let temporary = cache_root.join(STORE_TEMP_FILE);
    let path = cache_root.join(STORE_FILE);
    let backup = cache_root.join(STORE_BACKUP_FILE);
    fs::write(&temporary, encrypted).context("write pending mail rule store")?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temporary)
        .context("open pending mail rule store")?
        .sync_all()
        .context("flush pending mail rule store")?;
    if path.exists() {
        let _ = fs::remove_file(&backup);
        fs::rename(&path, &backup).context("backup mail rule store")?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        if backup.exists() {
            let _ = fs::rename(&backup, &path);
        }
        return Err(error).context("commit mail rule store");
    }
    Ok(())
}

fn store_snapshot(store: &RuleStore) -> RuleSnapshot {
    let mut rules = store.rules.clone();
    rules.sort_by_key(|rule| std::cmp::Reverse(rule.created_at));
    RuleSnapshot { rules }
}

pub fn snapshot(cache_root: &Path) -> Result<RuleSnapshot> {
    let mut cache = rule_cache()
        .lock()
        .map_err(|_| anyhow!("mail rule cache lock poisoned"))?;
    Ok(store_snapshot(&cached_store(cache_root, &mut cache)?))
}

pub fn add(
    cache_root: &Path,
    account_id: Option<String>,
    kind: MailRuleKind,
    value: &str,
) -> Result<(MailRule, RuleSnapshot)> {
    if let Some(account_id) = account_id.as_deref() {
        if !crate::valid_account_id(account_id) {
            return Err(anyhow!("mail rule account id is invalid"));
        }
    }
    let normalized = normalize_value(kind, value)?;
    let mut cache = rule_cache()
        .lock()
        .map_err(|_| anyhow!("mail rule cache lock poisoned"))?;
    let mut store = cached_store(cache_root, &mut cache)?;
    if let Some(existing) = store.rules.iter().find(|rule| {
        rule.account_id.as_deref().map(str::to_ascii_lowercase)
            == account_id.as_deref().map(str::to_ascii_lowercase)
            && rule.kind == kind
            && rule.value == normalized
    }) {
        return Ok((existing.clone(), store_snapshot(&store)));
    }
    if store.rules.len() >= MAX_RULES {
        return Err(anyhow!("mail rule store has reached its 256-rule limit"));
    }
    let created_at = now_millis();
    let id = loop {
        let candidate = format!("rule-{created_at}-{:016x}", rand::random::<u64>());
        if !store.rules.iter().any(|rule| rule.id == candidate) {
            break candidate;
        }
    };
    let rule = MailRule {
        id,
        account_id,
        kind,
        value: normalized,
        created_at,
    };
    validate_rule(&rule)?;
    store.rules.push(rule.clone());
    persist(cache_root, &store)?;
    cache.insert(cache_root.to_path_buf(), store.clone());
    Ok((rule, store_snapshot(&store)))
}

pub fn remove(cache_root: &Path, id: &str) -> Result<(bool, RuleSnapshot)> {
    if !valid_rule_id(id) {
        return Err(anyhow!("mail rule id is invalid"));
    }
    let mut cache = rule_cache()
        .lock()
        .map_err(|_| anyhow!("mail rule cache lock poisoned"))?;
    let mut store = cached_store(cache_root, &mut cache)?;
    let previous_len = store.rules.len();
    store.rules.retain(|rule| rule.id != id);
    let removed = store.rules.len() != previous_len;
    if removed {
        persist(cache_root, &store)?;
        cache.insert(cache_root.to_path_buf(), store.clone());
    }
    Ok((removed, store_snapshot(&store)))
}

pub fn remove_account(cache_root: &Path, account_id: &str) -> Result<usize> {
    if !crate::valid_account_id(account_id) {
        return Err(anyhow!("mail rule account id is invalid"));
    }
    let mut cache = rule_cache()
        .lock()
        .map_err(|_| anyhow!("mail rule cache lock poisoned"))?;
    let mut store = cached_store(cache_root, &mut cache)?;
    let previous_len = store.rules.len();
    store.rules.retain(|rule| {
        !rule
            .account_id
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(account_id))
    });
    let removed = previous_len.saturating_sub(store.rules.len());
    if removed > 0 {
        persist(cache_root, &store)?;
        cache.insert(cache_root.to_path_buf(), store);
    }
    Ok(removed)
}

fn sender_domain(sender: &str) -> Option<String> {
    normalize_sender(sender)
        .ok()
        .and_then(|value| value.rsplit_once('@').map(|(_, domain)| domain.to_string()))
}

pub fn rule_matches(rule: &MailRule, message: &CachedMessage) -> bool {
    if rule
        .account_id
        .as_deref()
        .is_some_and(|account_id| !account_id.eq_ignore_ascii_case(&message.account_id))
    {
        return false;
    }
    match rule.kind {
        MailRuleKind::Sender => {
            normalize_sender(&message.sender_email).is_ok_and(|sender| sender == rule.value)
        }
        MailRuleKind::Domain => sender_domain(&message.sender_email).is_some_and(|domain| {
            domain == rule.value || domain.ends_with(&format!(".{}", rule.value))
        }),
    }
}

pub fn is_blocked(snapshot: &RuleSnapshot, message: &CachedMessage) -> bool {
    snapshot
        .rules
        .iter()
        .any(|rule| rule_matches(rule, message))
}

pub fn apply_to_message(snapshot: &RuleSnapshot, message: &mut CachedMessage) {
    message.blocked = false;
    message.blocked_rule_id = None;
    if let Some(rule) = snapshot
        .rules
        .iter()
        .find(|rule| rule_matches(rule, message))
    {
        message.blocked = true;
        message.blocked_rule_id = Some(rule.id.clone());
    }
}

pub fn apply_to_messages(snapshot: &RuleSnapshot, messages: &mut [CachedMessage]) {
    for message in messages {
        apply_to_message(snapshot, message);
    }
}

#[cfg(test)]
fn clear_cached_root(cache_root: &Path) {
    if let Ok(mut cache) = rule_cache().lock() {
        cache.remove(cache_root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::SmartCategory;

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mailgo-rules-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    fn message(account_id: &str, sender_email: &str) -> CachedMessage {
        CachedMessage {
            id: "fixture".into(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_id: "fixture".into(),
            account_id: account_id.into(),
            folder: "INBOX".into(),
            uid: 1,
            subject: "Fixture".into(),
            sender_name: "Sender".into(),
            sender_email: sender_email.into(),
            to: Vec::new(),
            cc: Vec::new(),
            received_at: None,
            unread: true,
            starred: false,
            category: SmartCategory::Inbox,
            is_ad: false,
            blocked: false,
            blocked_rule_id: None,
            preview: String::new(),
            text_body: String::new(),
            html_body: None,
            attachments: Vec::new(),
            raw_path: None,
        }
    }

    #[test]
    fn encrypted_round_trip_deduplicates_and_removes() {
        let root = root("round-trip");
        let (first, first_snapshot) = add(
            &root,
            Some("account-1".into()),
            MailRuleKind::Sender,
            " NEWS@Example.COM ",
        )
        .unwrap();
        assert_eq!(first_snapshot.rules.len(), 1);
        assert_eq!(first_snapshot.rules[0].value, "news@example.com");
        let (duplicate, duplicate_snapshot) = add(
            &root,
            Some("account-1".into()),
            MailRuleKind::Sender,
            "news@example.com",
        )
        .unwrap();
        assert_eq!(duplicate.id, first.id);
        assert_eq!(duplicate_snapshot.rules.len(), 1);
        let encrypted = fs::read(root.join(STORE_FILE)).unwrap();
        assert!(!encrypted
            .windows(b"news@example.com".len())
            .any(|window| window == b"news@example.com"));
        assert!(remove(&root, &first.id).unwrap().0);
        assert!(snapshot(&root).unwrap().rules.is_empty());
        clear_cached_root(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sender_and_domain_rules_honor_account_scope() {
        let root = root("scope");
        add(
            &root,
            Some("account-1".into()),
            MailRuleKind::Sender,
            "exact@sender.example",
        )
        .unwrap();
        add(&root, None, MailRuleKind::Domain, "example.com").unwrap();
        let snapshot = snapshot(&root).unwrap();
        let mut exact = message("account-1", "EXACT@sender.example");
        let mut wrong_account = message("account-2", "exact@sender.example");
        let mut subdomain = message("account-2", "offer@news.example.com");
        apply_to_message(&snapshot, &mut exact);
        apply_to_message(&snapshot, &mut wrong_account);
        apply_to_message(&snapshot, &mut subdomain);
        assert!(exact.blocked);
        assert!(!wrong_account.blocked);
        assert!(subdomain.blocked);
        assert!(subdomain.blocked_rule_id.is_some());
        clear_cached_root(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn account_removal_preserves_global_and_other_account_rules() {
        let root = root("account-remove");
        add(
            &root,
            Some("account-1".into()),
            MailRuleKind::Domain,
            "one.example",
        )
        .unwrap();
        add(
            &root,
            Some("account-2".into()),
            MailRuleKind::Domain,
            "two.example",
        )
        .unwrap();
        add(&root, None, MailRuleKind::Domain, "global.example").unwrap();
        assert_eq!(remove_account(&root, "account-1").unwrap(), 1);
        let remaining = snapshot(&root).unwrap();
        assert_eq!(remaining.rules.len(), 2);
        assert!(remaining
            .rules
            .iter()
            .all(|rule| rule.account_id.as_deref() != Some("account-1")));
        clear_cached_root(&root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_values_and_oversized_stores_are_rejected() {
        assert!(normalize_value(MailRuleKind::Sender, "not-an-address").is_err());
        assert!(normalize_value(MailRuleKind::Domain, "bad domain.example").is_err());
        assert!(normalize_value(MailRuleKind::Domain, "-bad.example").is_err());

        let root = root("oversized");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(STORE_FILE), vec![0_u8; MAX_STORE_BYTES + 1]).unwrap();
        clear_cached_root(&root);
        assert!(snapshot(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_primary_recovers_previous_encrypted_backup() {
        let root = root("backup");
        add(&root, None, MailRuleKind::Domain, "first.example").unwrap();
        add(&root, None, MailRuleKind::Domain, "second.example").unwrap();
        fs::write(root.join(STORE_FILE), b"corrupt primary").unwrap();
        clear_cached_root(&root);
        let recovered = snapshot(&root).unwrap();
        assert_eq!(recovered.rules.len(), 1);
        assert_eq!(recovered.rules[0].value, "first.example");
        clear_cached_root(&root);
        let _ = fs::remove_dir_all(root);
    }
}
