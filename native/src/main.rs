#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use std::ptr::null;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::{distributions::Alphanumeric, Rng};
use rdesktop_core::config::{AppConfig, RendererConfig, WindowConfig, WindowIcon};
use rdesktop_core::ipc::{FnIpcHandler, IpcMessage, IpcResponse};
use rdesktop_core::renderer::Renderer;
use rdesktop_webview::WebViewRenderer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

mod cache_db;
mod classifier;
mod drafts;
mod instance;
mod mail;
mod oauth;
mod outbox;
mod providers;
mod send;
mod storage;
mod sync;
mod tray;

const APP_SERVICE: &str = "MailGo";
const CREDENTIAL_ENVELOPE_PREFIX: &str = "mailgo-credential-v1:";
const STATE_SCHEMA_VERSION: u32 = 1;
const ATTACHMENT_CHUNK_BYTES: usize = 192 * 1024;
const LOG_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

fn app_window_icon() -> Result<WindowIcon> {
    let image = image::load_from_memory_with_format(
        include_bytes!("../../resources/icons/mailgo-256.png"),
        image::ImageFormat::Png,
    )?
    .into_rgba8();
    let (width, height) = image.dimensions();
    Ok(WindowIcon {
        rgba: image.into_raw(),
        width,
        height,
    })
}

fn init_logging() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let log_dir = app_data_dir().join("logs");
    fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
    if let Ok(entries) = fs::read_dir(&log_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_mailgo_log = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mailgo.log."));
            let is_expired = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                .is_ok_and(|age| age > LOG_RETENTION);
            if is_mailgo_log && is_expired {
                let _ = fs::remove_file(path);
            }
        }
    }
    let appender = tracing_appender::rolling::daily(log_dir, "mailgo.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .compact()
        .try_init()
        .map_err(|error| anyhow!("initialize MailGo logging: {error}"))?;
    Ok(guard)
}
const MAX_ATTACHMENT_DOWNLOAD_BYTES: usize = 256 * 1024 * 1024;
const MAX_ACTIVE_ATTACHMENT_DOWNLOADS: usize = 2;
const ATTACHMENT_DOWNLOAD_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_OUTGOING_ATTACHMENT_BYTES: usize = drafts::MAX_ATTACHMENT_BYTES;
const MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES: usize = drafts::MAX_ATTACHMENT_TOTAL_BYTES;
const MAX_OUTGOING_ATTACHMENTS: usize = drafts::MAX_ATTACHMENTS;
const MAX_ACTIVE_ATTACHMENT_UPLOADS: usize = 4;
const ATTACHMENT_UPLOAD_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_IMPORTED_ACCOUNTS: usize = 64;
const MAX_AUTH_SESSIONS: usize = 16;
const MAX_FOLDERS_PER_ACCOUNT: usize = 64;
const MAX_ACCOUNT_ID_LENGTH: usize = 128;
const MAX_ACCOUNT_LABEL_LENGTH: usize = 128;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_RECIPIENT_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_MESSAGE_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_SEARCH_QUERY_BYTES: usize = 256;
const MAX_SEARCH_RESULTS: usize = 240;
const IPC_CAPABILITY_FIELD: &str = "__mailgoCapability";
const IPC_CAPABILITY_LENGTH: usize = 48;
const MAX_EXTERNAL_URL_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedAccount {
    id: String,
    provider: String,
    label: String,
    email: String,
    unread: u32,
    accent: String,
    status: String,
    last_sync: String,
    imap_host: Option<String>,
    imap_port: Option<u16>,
    imap_security: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    smtp_security: Option<String>,
    authentication: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    schema_version: u32,
    accounts: Vec<PersistedAccount>,
    #[serde(default)]
    folder_names: HashMap<String, Vec<String>>,
    theme: String,
    minimize_to_tray: bool,
    offline_mode: bool,
    #[serde(default = "default_notifications_enabled")]
    notifications_enabled: bool,
    #[serde(default)]
    remote_images_enabled: bool,
    #[serde(default = "default_hide_ads", alias = "hideAds")]
    hide_ads: bool,
}

#[derive(Debug, Deserialize)]
struct PersistedStateDisk {
    #[serde(default, alias = "schemaVersion")]
    schema_version: Option<u32>,
    #[serde(default)]
    accounts: Vec<PersistedAccount>,
    #[serde(default, alias = "folderNames")]
    folder_names: HashMap<String, Vec<String>>,
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default = "default_minimize_to_tray", alias = "minimizeToTray")]
    minimize_to_tray: bool,
    #[serde(default = "default_offline_mode", alias = "offlineMode")]
    offline_mode: bool,
    #[serde(
        default = "default_notifications_enabled",
        alias = "notificationsEnabled"
    )]
    notifications_enabled: bool,
    #[serde(default, alias = "remoteImagesEnabled")]
    remote_images_enabled: bool,
    #[serde(default = "default_hide_ads", alias = "hideAds")]
    hide_ads: bool,
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_minimize_to_tray() -> bool {
    true
}

fn default_offline_mode() -> bool {
    false
}

fn default_notifications_enabled() -> bool {
    true
}

fn default_hide_ads() -> bool {
    false
}

fn decode_persisted_state(contents: &str) -> Result<PersistedState> {
    let disk: PersistedStateDisk =
        serde_json::from_str(contents).context("parse MailGo persisted state")?;
    let version = disk.schema_version.unwrap_or(0);
    if version > STATE_SCHEMA_VERSION {
        return Err(anyhow!(
            "MailGo state schema {version} is newer than supported schema {STATE_SCHEMA_VERSION}"
        ));
    }
    let accounts = sanitize_persisted_accounts(disk.accounts);
    Ok(PersistedState {
        schema_version: STATE_SCHEMA_VERSION,
        folder_names: sanitize_persisted_folder_names(disk.folder_names, &accounts),
        accounts,
        theme: if disk.theme == "light" {
            "light".to_string()
        } else {
            "dark".to_string()
        },
        minimize_to_tray: disk.minimize_to_tray,
        offline_mode: disk.offline_mode,
        notifications_enabled: disk.notifications_enabled,
        remote_images_enabled: disk.remote_images_enabled,
        hide_ads: disk.hide_ads,
    })
}

fn sanitize_folder_names(folder_names: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for folder in folder_names {
        if folder.trim().is_empty()
            || folder.len() > 512
            || folder.chars().any(char::is_control)
            || normalized
                .iter()
                .any(|known: &String| known.eq_ignore_ascii_case(folder))
        {
            continue;
        }
        normalized.push(folder.clone());
        if normalized.len() == MAX_FOLDERS_PER_ACCOUNT {
            break;
        }
    }
    normalized
}

fn sanitize_persisted_folder_names(
    folder_names: HashMap<String, Vec<String>>,
    accounts: &[PersistedAccount],
) -> HashMap<String, Vec<String>> {
    let known_accounts = accounts
        .iter()
        .map(|account| account.id.as_str())
        .collect::<HashSet<_>>();
    folder_names
        .into_iter()
        .filter_map(|(account_id, folders)| {
            known_accounts
                .contains(account_id.as_str())
                .then(|| (account_id, sanitize_folder_names(&folders)))
        })
        .collect()
}

fn sanitize_persisted_accounts(accounts: Vec<PersistedAccount>) -> Vec<PersistedAccount> {
    let mut seen_ids = HashSet::new();
    accounts
        .into_iter()
        .filter(|account| {
            let normalized_id = account.id.to_ascii_lowercase();
            valid_account_id(&account.id)
                && account.label.len() <= MAX_ACCOUNT_LABEL_LENGTH
                && providers::validate_email(&account.email).is_ok()
                && profile_for_account(account).is_ok()
                && seen_ids.insert(normalized_id)
        })
        .take(MAX_IMPORTED_ACCOUNTS)
        .collect()
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            accounts: Vec::new(),
            folder_names: HashMap::new(),
            theme: "dark".to_string(),
            minimize_to_tray: true,
            offline_mode: false,
            notifications_enabled: true,
            remote_images_enabled: false,
            hide_ads: false,
        }
    }
}

struct MailGoState {
    state_path: PathBuf,
    state: PersistedState,
    auth_sessions: HashMap<String, oauth::PendingSession>,
    ready_oauth_credentials: HashMap<String, Zeroizing<String>>,
    attachment_downloads: HashMap<String, AttachmentDownloadSession>,
    attachment_uploads: HashMap<String, AttachmentUploadSession>,
    sync_in_flight: HashSet<String>,
    cache_scan: CacheScanState,
}

#[derive(Default)]
struct CacheScanState {
    generation: u64,
    running: bool,
    stats: Option<storage::CacheStats>,
    error: Option<String>,
}

struct AttachmentDownloadSession {
    bytes: Vec<u8>,
    created_at: Instant,
}

struct AttachmentUploadSession {
    file_name: String,
    content_type: String,
    content_id: Option<String>,
    expected_size: usize,
    bytes: Vec<u8>,
    created_at: Instant,
}

impl MailGoState {
    fn load() -> Result<Self> {
        let root = app_data_dir();
        fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        let state_path = root.join("state.json");
        let backup_path = root.join("state.json.bak");
        let state = match fs::read_to_string(&state_path) {
            Ok(contents) => match decode_persisted_state(&contents) {
                Ok(state) => state,
                Err(primary_error) => match fs::read_to_string(&backup_path) {
                    Ok(backup) => decode_persisted_state(&backup).with_context(|| {
                        format!("parse {} after {}", backup_path.display(), primary_error)
                    })?,
                    Err(_) => {
                        return Err(primary_error)
                            .with_context(|| format!("parse {}", state_path.display()))
                    }
                },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::read_to_string(&backup_path) {
                    Ok(backup) => decode_persisted_state(&backup)
                        .with_context(|| format!("parse {}", backup_path.display()))?,
                    Err(backup_error) if backup_error.kind() == std::io::ErrorKind::NotFound => {
                        PersistedState::default()
                    }
                    Err(backup_error) => {
                        return Err(backup_error)
                            .with_context(|| format!("read {}", backup_path.display()))
                    }
                }
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", state_path.display()))
            }
        };
        Ok(Self {
            state_path,
            state,
            auth_sessions: HashMap::new(),
            ready_oauth_credentials: HashMap::new(),
            attachment_downloads: HashMap::new(),
            attachment_uploads: HashMap::new(),
            sync_in_flight: HashSet::new(),
            cache_scan: CacheScanState::default(),
        })
    }

    fn save(&self) -> Result<()> {
        let temporary_path = self.state_path.with_extension("json.tmp");
        let backup_path = self.state_path.with_extension("json.bak");
        let payload = serde_json::to_vec_pretty(&self.state)?;
        fs::write(&temporary_path, payload)
            .with_context(|| format!("write {}", temporary_path.display()))?;
        if self.state_path.exists() {
            let _ = fs::remove_file(&backup_path);
            fs::rename(&self.state_path, &backup_path)
                .with_context(|| format!("backup {}", self.state_path.display()))?;
        }
        if let Err(error) = fs::rename(&temporary_path, &self.state_path) {
            let _ = fs::rename(&backup_path, &self.state_path);
            return Err(error).with_context(|| format!("commit {}", self.state_path.display()));
        }
        let _ = fs::remove_file(backup_path);
        Ok(())
    }

    fn snapshot(&self) -> Value {
        let folder_labels = self
            .state
            .folder_names
            .iter()
            .map(|(account_id, folders)| {
                let labels = folders
                    .iter()
                    .map(|folder| (folder.clone(), sync::folder_display_name(folder)))
                    .collect::<HashMap<_, _>>();
                (account_id.clone(), labels)
            })
            .collect::<HashMap<_, _>>();
        json!({
            "accounts": self.state.accounts,
            "folders": self.state.folder_names,
            "folderLabels": folder_labels,
            "theme": self.state.theme,
            "minimizeToTray": self.state.minimize_to_tray,
            "offlineMode": self.state.offline_mode,
            "notificationsEnabled": self.state.notifications_enabled,
            "remoteImagesEnabled": self.state.remote_images_enabled,
            "hideAds": self.state.hide_ads,
        })
    }
}

pub(crate) struct AccountSyncLease {
    shared: Arc<Mutex<MailGoState>>,
    account_id: String,
}

impl Drop for AccountSyncLease {
    fn drop(&mut self) {
        if let Ok(mut app) = self.shared.lock() {
            app.sync_in_flight.remove(&self.account_id);
        }
    }
}

fn reserve_account_sync(app: &mut MailGoState, account_id: &str) -> Result<()> {
    if !app
        .state
        .accounts
        .iter()
        .any(|account| account.id == account_id)
    {
        return Err(anyhow!("account not found"));
    }
    if !app.sync_in_flight.insert(account_id.to_string()) {
        return Err(anyhow!("account sync is already in progress"));
    }
    Ok(())
}

pub(crate) fn try_begin_account_sync(
    shared: &Arc<Mutex<MailGoState>>,
    account_id: &str,
) -> Result<AccountSyncLease> {
    let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    reserve_account_sync(&mut app, account_id)?;
    Ok(AccountSyncLease {
        shared: Arc::clone(shared),
        account_id: account_id.to_string(),
    })
}

fn app_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("MailGo"))
        .join("MailGo")
}

fn webview_data_dir() -> PathBuf {
    app_data_dir().join("WebView2")
}

fn dist_root() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(configured) = std::env::var_os("MAILGO_DIST_DIR") {
        candidates.push(PathBuf::from(configured));
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join("dist"));
        }
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../dist"));

    for candidate in candidates {
        if candidate.join("index.html").is_file() {
            return candidate
                .canonicalize()
                .context("canonicalize MailGo renderer assets");
        }
    }
    Err(anyhow!(
        "MailGo renderer assets are missing; run npm run build or place dist next to the executable"
    ))
}

fn validate_external_url(value: &str) -> Result<url::Url> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_EXTERNAL_URL_BYTES {
        return Err(anyhow!("external URL is empty or too long"));
    }
    let parsed = url::Url::parse(value).context("invalid external URL")?;
    let is_https = parsed.scheme() == "https"
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none();
    let is_mailto = parsed.scheme() == "mailto"
        && parsed.host_str().is_none()
        && !parsed.path().trim().is_empty()
        && !value.to_ascii_lowercase().contains("%0d")
        && !value.to_ascii_lowercase().contains("%0a");
    if !is_https && !is_mailto {
        return Err(anyhow!(
            "external links must use HTTPS or mailto without embedded credentials"
        ));
    }
    Ok(parsed)
}

#[cfg(target_os = "windows")]
fn open_external_url(value: &str) -> Result<()> {
    let parsed = validate_external_url(value)?;
    let wide = parsed
        .as_str()
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            0,
            windows_sys::core::w!("open"),
            wide.as_ptr(),
            null(),
            null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    if result <= 32 {
        return Err(anyhow!("Windows could not open the external URL"));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn open_external_url(value: &str) -> Result<()> {
    let _ = validate_external_url(value)?;
    Err(anyhow!(
        "native external URL opening is only available on Windows"
    ))
}

fn cache_dir() -> PathBuf {
    app_data_dir().join("cache")
}

fn credential_entry(account_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(APP_SERVICE, account_id)
        .map_err(|error| anyhow!("credential store unavailable: {error}"))
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCredential {
    schema_version: u32,
    binding: String,
    credential: Zeroizing<String>,
}

fn credential_binding(account: &PersistedAccount) -> Result<String> {
    let profile = profile_for_account(account)?;
    let canonical = serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "accountId": account.id.to_ascii_lowercase(),
        "provider": profile.provider.as_str(),
        "email": account.email.trim().to_ascii_lowercase(),
        "authentication": profile.authentication,
        "imap": {
            "host": profile.imap.host.trim().to_ascii_lowercase(),
            "port": profile.imap.port,
            "security": profile.imap.security,
        },
        "smtp": {
            "host": profile.smtp.host.trim().to_ascii_lowercase(),
            "port": profile.smtp.port,
            "security": profile.smtp.security,
        },
    }))?;
    Ok(STANDARD.encode(Sha256::digest(canonical)))
}

fn encode_stored_credential(
    account: &PersistedAccount,
    credential: &str,
) -> Result<Zeroizing<String>> {
    let envelope = StoredCredential {
        schema_version: 1,
        binding: credential_binding(account)?,
        credential: Zeroizing::new(credential.to_string()),
    };
    let serialized = Zeroizing::new(serde_json::to_string(&envelope)?);
    let mut stored = Zeroizing::new(String::with_capacity(
        CREDENTIAL_ENVELOPE_PREFIX.len() + serialized.len(),
    ));
    stored.push_str(CREDENTIAL_ENVELOPE_PREFIX);
    stored.push_str(serialized.as_str());
    Ok(stored)
}

fn decode_stored_credential(
    account: &PersistedAccount,
    stored: &str,
) -> Result<Option<Zeroizing<String>>> {
    let Some(serialized) = stored.strip_prefix(CREDENTIAL_ENVELOPE_PREFIX) else {
        return Ok(None);
    };
    let envelope: StoredCredential = serde_json::from_str(serialized)
        .map_err(|_| anyhow!("stored credential is unreadable; reauthorization required"))?;
    if envelope.schema_version != 1 || envelope.binding != credential_binding(account)? {
        return Err(anyhow!(
            "account connection settings changed; reauthorization required"
        ));
    }
    Ok(Some(envelope.credential))
}

fn store_credential(account: &PersistedAccount, credential: &str) -> Result<()> {
    let stored = encode_stored_credential(account, credential)?;
    credential_entry(&account.id)?
        .set_password(stored.as_str())
        .map_err(|error| anyhow!("save credential: {error}"))
}

fn legacy_credential_migration_allowed(account: &PersistedAccount) -> Result<bool> {
    Ok(providers::ProviderKind::parse(&account.provider)? != providers::ProviderKind::Other)
}

fn snapshot_credential(account_id: &str) -> Result<Option<Zeroizing<String>>> {
    match credential_entry(account_id)?.get_password() {
        Ok(value) => Ok(Some(Zeroizing::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(anyhow!("credential store unavailable: {error}")),
    }
}

fn delete_credential_if_present(account_id: &str) -> Result<()> {
    match credential_entry(account_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow!("remove credential: {error}")),
    }
}

fn load_credential(account: &PersistedAccount) -> Result<Zeroizing<String>> {
    let entry = credential_entry(&account.id)?;
    let stored = Zeroizing::new(entry.get_password().map_err(|error| match error {
        keyring::Error::NoEntry => {
            anyhow!("account credential is missing; reauthorization required")
        }
        error => anyhow!("credential store unavailable: {error}"),
    })?);
    let (raw, requires_envelope_migration) =
        match decode_stored_credential(account, stored.as_str())? {
            Some(credential) => (credential, false),
            None if legacy_credential_migration_allowed(account)? => {
                (Zeroizing::new(stored.to_string()), true)
            }
            None => {
                return Err(anyhow!(
                    "legacy custom account credentials require reauthorization before connecting"
                ))
            }
        };
    let provider = providers::ProviderKind::parse(&account.provider)?;
    let refreshed = oauth::refresh_if_needed(provider, raw.as_str())?;
    if refreshed.as_str() != raw.as_str() || requires_envelope_migration {
        let encoded = encode_stored_credential(account, refreshed.as_str())?;
        entry
            .set_password(encoded.as_str())
            .map_err(|error| anyhow!("save refreshed credential: {error}"))?;
    }
    Ok(refreshed)
}

fn account_for(shared: &Arc<Mutex<MailGoState>>, account_id: &str) -> Result<PersistedAccount> {
    shared
        .lock()
        .map_err(|_| anyhow!("state lock poisoned"))?
        .state
        .accounts
        .iter()
        .find(|account| account.id == account_id)
        .cloned()
        .ok_or_else(|| anyhow!("account not found"))
}

fn offline_mode_enabled(shared: &Arc<Mutex<MailGoState>>) -> Result<bool> {
    Ok(shared
        .lock()
        .map_err(|_| anyhow!("state lock poisoned"))?
        .state
        .offline_mode)
}

fn ensure_network_allowed(shared: &Arc<Mutex<MailGoState>>) -> Result<()> {
    if offline_mode_enabled(shared)? {
        return Err(anyhow!(
            "MailGo is in offline-only mode; turn off offline mode before connecting"
        ));
    }
    Ok(())
}

pub(crate) fn record_account_sync_failure(
    shared: &Arc<Mutex<MailGoState>>,
    account_id: &str,
    needs_auth: bool,
) {
    let Ok(mut app) = shared.lock() else {
        tracing::warn!(account_id = %account_id, "background sync status lock poisoned");
        return;
    };
    if let Some(account) = app
        .state
        .accounts
        .iter_mut()
        .find(|account| account.id == account_id)
    {
        account.status = if needs_auth {
            "needs-auth".into()
        } else {
            "offline".into()
        };
        account.last_sync = if needs_auth {
            "等待重新授权".into()
        } else {
            "后台同步失败，可重试".into()
        };
        if let Err(error) = app.save() {
            tracing::warn!(account_id = %account_id, "background sync status save failed: {error}");
        }
    }
}

fn record_account_sync_success(app: &mut MailGoState, result: &sync::SyncResult) {
    record_account_sync_success_with_mode(app, result, true);
}

fn record_account_sync_success_with_mode(
    app: &mut MailGoState,
    result: &sync::SyncResult,
    replace_folders: bool,
) {
    if let Some(account) = app
        .state
        .accounts
        .iter_mut()
        .find(|item| item.id == result.account_id)
    {
        account.unread = result.unread as u32;
        account.status = "synced".to_string();
        account.last_sync = "刚刚同步".to_string();
    }
    let discovered = sanitize_folder_names(&result.folders);
    if replace_folders || !app.state.folder_names.contains_key(&result.account_id) {
        app.state
            .folder_names
            .insert(result.account_id.clone(), discovered);
    } else if let Some(existing) = app.state.folder_names.get_mut(&result.account_id) {
        existing.extend(discovered);
        *existing = sanitize_folder_names(existing);
    }
}

fn profile_for_account(account: &PersistedAccount) -> Result<providers::ProviderProfile> {
    let provider = providers::ProviderKind::parse(&account.provider)?;
    if provider != providers::ProviderKind::Other {
        let mut profile = providers::profile_for(provider)?;
        if let Some(authentication) = account.authentication.as_deref() {
            let authentication = providers::Authentication::parse(authentication)?;
            if authentication == providers::Authentication::OAuth2 && !profile.supports_oauth {
                return Err(anyhow!("this provider does not support OAuth2 in MailGo"));
            }
            profile.authentication = authentication;
        }
        return Ok(profile);
    }
    providers::profile_for_custom(&providers::CustomConnectionSettings {
        imap_host: account
            .imap_host
            .clone()
            .ok_or_else(|| anyhow!("custom IMAP host is required"))?,
        imap_port: account
            .imap_port
            .ok_or_else(|| anyhow!("custom IMAP port is required"))?,
        imap_security: providers::TransportSecurity::parse(
            account.imap_security.as_deref().unwrap_or("tls"),
        )?,
        smtp_host: account
            .smtp_host
            .clone()
            .ok_or_else(|| anyhow!("custom SMTP host is required"))?,
        smtp_port: account
            .smtp_port
            .ok_or_else(|| anyhow!("custom SMTP port is required"))?,
        smtp_security: providers::TransportSecurity::parse(
            account.smtp_security.as_deref().unwrap_or("tls"),
        )?,
        authentication: providers::Authentication::parse(
            account.authentication.as_deref().unwrap_or("password"),
        )?,
    })
}

enum ManualAccountSyncOutcome {
    Synced(Value),
    Failed(Value),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionDiagnosticChannel {
    ok: bool,
    status: &'static str,
    latency_ms: u64,
}

fn connection_diagnostic_channel(
    account_id: &str,
    channel: &'static str,
    elapsed: Duration,
    result: Result<()>,
    categorize: impl FnOnce(&anyhow::Error) -> &'static str,
) -> ConnectionDiagnosticChannel {
    let latency_ms = u64::try_from(elapsed.as_millis())
        .unwrap_or(u64::MAX)
        .min(120_000);
    match result {
        Ok(()) => ConnectionDiagnosticChannel {
            ok: true,
            status: "ok",
            latency_ms,
        },
        Err(error) => {
            let status = categorize(&error);
            tracing::warn!(
                account_id,
                channel,
                category = status,
                "account connection diagnostic failed"
            );
            ConnectionDiagnosticChannel {
                ok: false,
                status,
                latency_ms,
            }
        }
    }
}

fn manual_sync_failure(account_id: &str, message: impl Into<String>) -> ManualAccountSyncOutcome {
    ManualAccountSyncOutcome::Failed(json!({
        "accountId": account_id,
        "message": message.into(),
    }))
}

fn run_manual_account_sync(
    shared: &Arc<Mutex<MailGoState>>,
    cache_root: &Path,
    account: &PersistedAccount,
) -> ManualAccountSyncOutcome {
    let _sync_lease = match try_begin_account_sync(shared, &account.id) {
        Ok(lease) => lease,
        Err(error) => {
            tracing::warn!(account_id = %account.id, category = "concurrency", "account sync skipped");
            return manual_sync_failure(&account.id, error.to_string());
        }
    };
    let profile = match profile_for_account(account) {
        Ok(profile) => profile,
        Err(error) => {
            record_account_sync_failure(shared, &account.id, sync::needs_reauthorization(&error));
            tracing::warn!(account_id = %account.id, category = "configuration", "account sync skipped");
            return manual_sync_failure(&account.id, error.to_string());
        }
    };
    let credential = match load_credential(account) {
        Ok(credential) => credential,
        Err(error) => {
            let needs_auth = sync::needs_reauthorization(&error);
            record_account_sync_failure(shared, &account.id, needs_auth);
            tracing::warn!(
                account_id = %account.id,
                category = if needs_auth { "authentication" } else { "credential-store" },
                provider = profile.provider.as_str(),
                "account sync credential unavailable"
            );
            return manual_sync_failure(
                &account.id,
                if needs_auth {
                    "requires authorization"
                } else {
                    "credential store unavailable"
                },
            );
        }
    };
    if let Err(error) = outbox::flush_due(
        cache_root,
        &account.id,
        profile.clone(),
        &account.email,
        &credential,
    ) {
        tracing::warn!(account_id = %account.id, "all-account outbox flush failed: {error}");
    }
    let provider = profile.provider;
    match sync::sync_account(
        &account.id,
        profile,
        &account.email,
        &credential,
        cache_root,
    ) {
        Ok(result) => {
            let serialized = match serde_json::to_value(&result) {
                Ok(serialized) => serialized,
                Err(error) => {
                    tracing::warn!(account_id = %account.id, "serialize sync result failed: {error}");
                    return manual_sync_failure(&account.id, "could not prepare sync result");
                }
            };
            let state_result = (|| -> Result<()> {
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                record_account_sync_success(&mut app, &result);
                app.save()
            })();
            if let Err(error) = state_result {
                tracing::warn!(account_id = %account.id, "persist synchronized account state failed: {error}");
                return manual_sync_failure(
                    &account.id,
                    "could not persist synchronized account state",
                );
            }
            tracing::info!(
                account_id = %account.id,
                provider = provider.as_str(),
                unread = result.unread,
                "account sync completed"
            );
            ManualAccountSyncOutcome::Synced(serialized)
        }
        Err(error) => {
            let category = sync::error_category(&error, provider);
            let detail = sync::error_detail(&error);
            record_account_sync_failure(shared, &account.id, sync::needs_reauthorization(&error));
            tracing::warn!(
                account_id = %account.id,
                provider = provider.as_str(),
                category,
                detail,
                "account sync failed"
            );
            manual_sync_failure(&account.id, error.to_string())
        }
    }
}

fn string_field(payload: &Value, name: &str) -> Result<String> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("missing or empty field: {name}"))
}

fn bounded_string_field(payload: &Value, name: &str, max_bytes: usize) -> Result<String> {
    let value = string_field(payload, name)?;
    if value.len() > max_bytes {
        return Err(anyhow!("field {name} exceeds the safe size limit"));
    }
    Ok(value)
}

fn text_field(payload: &Value, name: &str, max_bytes: usize) -> Result<String> {
    let value = payload
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing field: {name}"))?;
    if value.len() > max_bytes {
        return Err(anyhow!("field {name} exceeds the safe size limit"));
    }
    Ok(value.to_string())
}

fn u32_field(payload: &Value, name: &str) -> Result<u32> {
    payload
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| anyhow!("missing or invalid field: {name}"))
}

fn optional_string_field(payload: &Value, name: &str) -> Option<String> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn optional_bounded_string_field(
    payload: &Value,
    name: &str,
    max_bytes: usize,
) -> Result<Option<String>> {
    let Some(value) = optional_string_field(payload, name) else {
        return Ok(None);
    };
    if value.len() > max_bytes {
        return Err(anyhow!("field {name} exceeds the safe size limit"));
    }
    Ok(Some(value))
}

fn thread_header_fields(payload: &Value) -> Result<(Option<String>, Vec<String>)> {
    let in_reply_to =
        optional_bounded_string_field(payload, "inReplyTo", mail::MAX_MESSAGE_ID_BYTES)?
            .map(|value| {
                mail::safe_message_id(&value).ok_or_else(|| anyhow!("invalid reply message id"))
            })
            .transpose()?;
    let references = match payload.get("references") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => {
            let values = value
                .as_array()
                .ok_or_else(|| anyhow!("references must be an array"))?;
            if values.len() > mail::MAX_THREAD_REFERENCES {
                return Err(anyhow!("reply contains too many message references"));
            }
            values
                .iter()
                .map(|value| {
                    let value = value
                        .as_str()
                        .ok_or_else(|| anyhow!("reply reference must be a string"))?;
                    mail::safe_message_id(value).ok_or_else(|| anyhow!("invalid reply reference"))
                })
                .collect::<Result<Vec<_>>>()?
        }
    };
    send::validate_thread_headers(in_reply_to.as_deref(), &references)?;
    Ok((in_reply_to, references))
}

fn optional_u16_field(payload: &Value, name: &str) -> Option<u16> {
    payload
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
}

fn valid_account_id(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or_default();
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= MAX_ACCOUNT_ID_LENGTH
        && !value.ends_with('.')
        && !matches!(
            stem.to_ascii_lowercase().as_str(),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        )
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn has_case_variant_account_id(accounts: &[PersistedAccount], id: &str) -> bool {
    accounts
        .iter()
        .any(|account| account.id != id && account.id.eq_ignore_ascii_case(id))
}

fn account_identity_matches(existing: &PersistedAccount, proposed: &PersistedAccount) -> bool {
    existing.provider.eq_ignore_ascii_case(&proposed.provider)
        && existing.email.eq_ignore_ascii_case(&proposed.email)
        && (!existing.provider.eq_ignore_ascii_case("other")
            || (existing
                .imap_host
                .as_deref()
                .unwrap_or_default()
                .eq_ignore_ascii_case(proposed.imap_host.as_deref().unwrap_or_default())
                && existing.imap_port == proposed.imap_port
                && existing
                    .imap_security
                    .as_deref()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(proposed.imap_security.as_deref().unwrap_or_default())
                && existing
                    .smtp_host
                    .as_deref()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(proposed.smtp_host.as_deref().unwrap_or_default())
                && existing.smtp_port == proposed.smtp_port
                && existing
                    .smtp_security
                    .as_deref()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(proposed.smtp_security.as_deref().unwrap_or_default())))
}

fn has_new_mailbox_identity_conflict(
    existing: &[PersistedAccount],
    incoming: &[PersistedAccount],
) -> bool {
    incoming.iter().enumerate().any(|(index, proposed)| {
        if existing.iter().any(|account| account.id == proposed.id) {
            return false;
        }
        existing
            .iter()
            .any(|account| account_identity_matches(account, proposed))
            || incoming[..index]
                .iter()
                .any(|account| account_identity_matches(account, proposed))
    })
}

fn has_existing_account_identity_change(
    existing: &[PersistedAccount],
    incoming: &[PersistedAccount],
) -> bool {
    incoming.iter().any(|proposed| {
        existing
            .iter()
            .find(|account| account.id == proposed.id)
            .is_some_and(|account| !account_identity_matches(account, proposed))
    })
}

fn clear_credential_snapshots(previous: &mut [(String, Option<Zeroizing<String>>)]) {
    for (_, credential) in previous {
        if let Some(value) = credential {
            value.zeroize();
        }
    }
}

fn restore_credentials(previous: &mut [(String, Option<Zeroizing<String>>)]) {
    for (id, credential) in &mut *previous {
        if let Ok(entry) = credential_entry(id) {
            match credential {
                Some(value) => {
                    let _ = entry.set_password(value.as_str());
                }
                None => {
                    let _ = entry.delete_credential();
                }
            }
        }
    }
    clear_credential_snapshots(previous);
}

fn purge_expired_attachment_downloads(app: &mut MailGoState) {
    app.attachment_downloads
        .retain(|_, download| download.created_at.elapsed() < ATTACHMENT_DOWNLOAD_TTL);
}

fn purge_expired_attachment_uploads(app: &mut MailGoState) {
    app.attachment_uploads
        .retain(|_, upload| upload.created_at.elapsed() < ATTACHMENT_UPLOAD_TTL);
}

fn outgoing_upload_bytes(app: &MailGoState) -> usize {
    app.attachment_uploads
        .values()
        .map(|upload| upload.expected_size)
        .sum()
}

fn valid_upload_file_name(value: &str) -> Result<String> {
    let name = value.trim();
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n' | '/' | '\\'))
    {
        return Err(anyhow!("invalid attachment file name"));
    }
    Ok(name.to_string())
}

fn valid_upload_content_type(value: Option<String>) -> Result<String> {
    let content_type = value
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    if content_type.len() > 128
        || content_type
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
    {
        return Err(anyhow!("invalid attachment content type"));
    }
    Ok(content_type)
}

fn valid_upload_content_id(value: Option<String>) -> Result<Option<String>> {
    let Some(content_id) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if content_id.len() > 128
        || !content_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '@')
        })
    {
        return Err(anyhow!("invalid inline attachment content id"));
    }
    Ok(Some(content_id))
}

fn purge_expired_auth_sessions(app: &mut MailGoState) {
    let expired_ids = app
        .auth_sessions
        .iter()
        .filter(|(_, session)| oauth::is_expired(session))
        .map(|(session_id, _)| session_id.clone())
        .collect::<Vec<_>>();
    for session_id in expired_ids {
        if let Some(session) = app.auth_sessions.remove(&session_id) {
            oauth::cancel(&session);
        }
    }
    let active_sessions = app.auth_sessions.keys().cloned().collect::<HashSet<_>>();
    app.ready_oauth_credentials
        .retain(|session_id, _| active_sessions.contains(session_id));
}

fn cancel_auth_session(app: &mut MailGoState, session_id: &str) -> bool {
    let pending = app.auth_sessions.remove(session_id);
    let had_pending = pending.is_some();
    if let Some(session) = pending {
        oauth::cancel(&session);
    }
    let had_ready_credential = app.ready_oauth_credentials.remove(session_id).is_some();
    had_pending || had_ready_credential
}

fn ensure_auth_session_capacity(app: &mut MailGoState) -> Result<()> {
    purge_expired_auth_sessions(app);
    if app.auth_sessions.len() >= MAX_AUTH_SESSIONS {
        return Err(anyhow!(
            "too many pending authorization sessions; finish or restart one before continuing"
        ));
    }
    Ok(())
}

fn account_capacity_available(accounts: &[PersistedAccount], id: &str) -> bool {
    accounts.iter().any(|account| account.id == id) || accounts.len() < MAX_IMPORTED_ACCOUNTS
}

fn import_fits_account_capacity(
    existing: &[PersistedAccount],
    incoming_ids: &HashSet<String>,
) -> bool {
    let retained = existing
        .iter()
        .filter(|account| {
            !incoming_ids
                .iter()
                .any(|incoming| incoming.eq_ignore_ascii_case(&account.id))
        })
        .count();
    retained.saturating_add(incoming_ids.len()) <= MAX_IMPORTED_ACCOUNTS
}

fn attachment_chunk_bounds(total: usize, offset: usize) -> Result<(usize, bool)> {
    if offset > total {
        return Err(anyhow!("attachment download offset is invalid"));
    }
    let next_offset = offset.saturating_add(ATTACHMENT_CHUNK_BYTES).min(total);
    Ok((next_offset, next_offset == total))
}

fn response(message: &IpcMessage, success: bool, data: Value) -> IpcResponse {
    IpcResponse {
        id: message.id.clone(),
        success,
        data,
    }
}

fn generate_ipc_capability() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(IPC_CAPABILITY_LENGTH)
        .map(char::from)
        .collect()
}

fn ipc_capability_is_well_formed(value: &str) -> bool {
    value.len() == IPC_CAPABILITY_LENGTH && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn cache_stats_response(state: &CacheScanState) -> Value {
    json!({
        "state": if state.running { "loading" } else if state.error.is_some() { "error" } else { "ready" },
        "stats": &state.stats,
        "message": &state.error,
    })
}

fn request_cache_stats(shared: &Arc<Mutex<MailGoState>>, refresh: bool) -> Result<Value> {
    let generation = {
        let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        if !app.cache_scan.running && (refresh || app.cache_scan.stats.is_none()) {
            app.cache_scan.generation = app.cache_scan.generation.saturating_add(1);
            app.cache_scan.running = true;
            app.cache_scan.error = None;
            Some(app.cache_scan.generation)
        } else {
            None
        }
    };

    if let Some(generation) = generation {
        let state_for_scan = Arc::clone(shared);
        let root = cache_dir();
        if let Err(error) = std::thread::Builder::new()
            .name("mailgo-cache-stats".to_string())
            .spawn(move || {
                let stats = storage::measure(&root);
                if let Ok(mut app) = state_for_scan.lock() {
                    if app.cache_scan.generation == generation {
                        app.cache_scan.stats = Some(stats);
                        app.cache_scan.running = false;
                        app.cache_scan.error = None;
                    }
                }
            })
        {
            tracing::warn!("cache statistics worker could not start: {error}");
            if let Ok(mut app) = shared.lock() {
                if app.cache_scan.generation == generation {
                    app.cache_scan.running = false;
                    app.cache_scan.error = Some("缓存统计暂不可用".to_string());
                }
            }
        }
    }

    let app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
    Ok(cache_stats_response(&app.cache_scan))
}

fn validate_ipc_capability(message: &IpcMessage, expected: &str) -> Result<()> {
    let received = message
        .payload
        .get(IPC_CAPABILITY_FIELD)
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !ipc_capability_is_well_formed(received)
        || !ipc_capability_is_well_formed(expected)
        || received
            .bytes()
            .zip(expected.bytes())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            != 0
    {
        return Err(anyhow!("native IPC caller is not trusted"));
    }
    Ok(())
}

fn handle_ipc(
    shared: &Arc<Mutex<MailGoState>>,
    message: IpcMessage,
    ipc_capability: &str,
) -> IpcResponse {
    let result = (|| -> Result<Value> {
        validate_ipc_capability(&message, ipc_capability)?;
        match message.cmd.as_str() {
            "app.get_state" => Ok(shared
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?
                .snapshot()),
            "app.cache_stats" => request_cache_stats(
                shared,
                message
                    .payload
                    .get("refresh")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            "app.set_minimize_to_tray" => {
                let enabled = message
                    .payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.minimize_to_tray = enabled;
                app.save()?;
                tray::set_minimize_to_tray(enabled);
                Ok(json!({ "enabled": enabled }))
            }
            "app.set_notifications" => {
                let enabled = message
                    .payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.notifications_enabled = enabled;
                app.save()?;
                Ok(json!({ "enabled": enabled }))
            }
            "app.set_offline_mode" => {
                let enabled = message
                    .payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.offline_mode = enabled;
                app.save()?;
                Ok(json!({ "enabled": enabled }))
            }
            "app.set_remote_images" => {
                let enabled = message
                    .payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.remote_images_enabled = enabled;
                app.save()?;
                Ok(json!({ "enabled": enabled }))
            }
            "app.set_hide_ads" => {
                let enabled = message
                    .payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.hide_ads = enabled;
                app.save()?;
                Ok(json!({ "enabled": enabled }))
            }
            "app.hide_window" => {
                tray::hide_main_window();
                Ok(json!({ "hidden": true }))
            }
            "app.open_external" => {
                let url = text_field(&message.payload, "url", MAX_EXTERNAL_URL_BYTES)?;
                open_external_url(&url)?;
                Ok(json!({ "opened": true }))
            }
            "app.set_theme" => {
                let theme = string_field(&message.payload, "theme")?;
                if theme != "dark" && theme != "light" {
                    return Err(anyhow!("unsupported theme"));
                }
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.theme = theme.clone();
                app.save()?;
                Ok(json!({ "theme": theme }))
            }
            "auth.cancel" => {
                let session_id = bounded_string_field(&message.payload, "sessionId", 128)?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                let cancelled = cancel_auth_session(&mut app, &session_id);
                Ok(json!({ "sessionId": session_id, "cancelled": cancelled }))
            }
            "auth.start" => {
                let provider =
                    providers::ProviderKind::parse(&string_field(&message.payload, "provider")?)?;
                let email = string_field(&message.payload, "email")?.trim().to_string();
                providers::validate_email(&email)?;
                {
                    let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                    ensure_auth_session_capacity(&mut app)?;
                }
                let (session, response) = oauth::start(provider, &email)?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                if let Err(error) = ensure_auth_session_capacity(&mut app) {
                    oauth::cancel(&session);
                    return Err(error);
                }
                app.auth_sessions.insert(session.id.clone(), session);
                Ok(serde_json::to_value(response)?)
            }
            "auth.device.start" => {
                let provider =
                    providers::ProviderKind::parse(&string_field(&message.payload, "provider")?)?;
                let email = string_field(&message.payload, "email")?;
                providers::validate_email(&email)?;
                {
                    let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                    ensure_auth_session_capacity(&mut app)?;
                }
                let (session, response) = oauth::start_device(provider, &email)?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                ensure_auth_session_capacity(&mut app)?;
                app.auth_sessions.insert(session.id.clone(), session);
                Ok(serde_json::to_value(response)?)
            }
            "auth.device.poll" => {
                let session_id = string_field(&message.payload, "sessionId")?;
                let (pending, ready) = {
                    let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                    purge_expired_auth_sessions(&mut app);
                    (
                        app.auth_sessions.get(&session_id).cloned(),
                        app.ready_oauth_credentials.contains_key(&session_id),
                    )
                };
                if ready {
                    return Ok(json!({ "sessionId": session_id, "status": "complete" }));
                }
                let pending =
                    pending.ok_or_else(|| anyhow!("OAuth device session is missing or expired"))?;
                match oauth::poll_device(&pending)? {
                    oauth::DevicePollResult::Pending { retry_after } => Ok(json!({
                        "sessionId": session_id,
                        "status": "pending",
                        "retryAfter": retry_after,
                    })),
                    oauth::DevicePollResult::Complete { credential } => {
                        let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                        app.ready_oauth_credentials
                            .insert(session_id.clone(), credential);
                        Ok(json!({ "sessionId": session_id, "status": "complete" }))
                    }
                }
            }
            "accounts.add" => {
                let id = string_field(&message.payload, "id")?;
                if !valid_account_id(&id) {
                    return Err(anyhow!("invalid account id"));
                }
                let provider = string_field(&message.payload, "provider")?;
                let label = string_field(&message.payload, "label")?;
                if label.len() > MAX_ACCOUNT_LABEL_LENGTH {
                    return Err(anyhow!("account label is too long"));
                }
                let email = string_field(&message.payload, "email")?;
                let supplied_credential = Zeroizing::new(
                    optional_bounded_string_field(
                        &message.payload,
                        "authorizationCode",
                        MAX_CREDENTIAL_BYTES,
                    )?
                    .unwrap_or_default(),
                );
                let provider_kind = providers::ProviderKind::parse(&provider)?;
                providers::validate_email(&email)?;
                let offline_mode = offline_mode_enabled(shared)?;
                let new_account = PersistedAccount {
                    id: id.clone(),
                    provider: provider_kind.as_str().to_string(),
                    label,
                    email,
                    unread: 0,
                    accent: "#5f70ee".to_string(),
                    status: if offline_mode {
                        "offline".to_string()
                    } else {
                        "synced".to_string()
                    },
                    last_sync: if offline_mode {
                        "仅离线模式".to_string()
                    } else {
                        "刚刚同步".to_string()
                    },
                    imap_host: optional_string_field(&message.payload, "imapHost"),
                    imap_port: optional_u16_field(&message.payload, "imapPort"),
                    imap_security: optional_string_field(&message.payload, "imapSecurity"),
                    smtp_host: optional_string_field(&message.payload, "smtpHost"),
                    smtp_port: optional_u16_field(&message.payload, "smtpPort"),
                    smtp_security: optional_string_field(&message.payload, "smtpSecurity"),
                    authentication: optional_string_field(&message.payload, "authentication"),
                };
                let profile = profile_for_account(&new_account)?;
                let account_is_existing = {
                    let app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                    if has_case_variant_account_id(&app.state.accounts, &id) {
                        return Err(anyhow!(
                            "account id differs only by case from an existing account"
                        ));
                    }
                    if let Some(existing) =
                        app.state.accounts.iter().find(|account| account.id == id)
                    {
                        if !account_identity_matches(existing, &new_account) {
                            return Err(anyhow!(
                                "account identity is fixed; remove the account and add it again to change mailbox"
                            ));
                        }
                    }
                    if has_new_mailbox_identity_conflict(
                        &app.state.accounts,
                        std::slice::from_ref(&new_account),
                    ) {
                        return Err(anyhow!(
                            "mailbox is already configured under another account"
                        ));
                    }
                    if !account_capacity_available(&app.state.accounts, &id) {
                        return Err(anyhow!(
                            "MailGo supports at most {MAX_IMPORTED_ACCOUNTS} accounts"
                        ));
                    }
                    app.state.accounts.iter().any(|account| account.id == id)
                };
                let _account_sync_lease = if account_is_existing {
                    Some(try_begin_account_sync(shared, &id)?)
                } else {
                    None
                };

                let oauth_session_id = optional_string_field(&message.payload, "oauthSessionId");
                let credential = if let Some(session_id) = oauth_session_id
                    .filter(|_| profile.authentication == providers::Authentication::OAuth2)
                {
                    let returned_state = optional_string_field(&message.payload, "oauthState");
                    let (pending, ready_credential) = {
                        let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                        purge_expired_auth_sessions(&mut app);
                        let pending = app.auth_sessions.remove(&session_id).ok_or_else(|| {
                            anyhow!("OAuth sign-in session is missing or expired")
                        })?;
                        oauth::cancel(&pending);
                        (pending, app.ready_oauth_credentials.remove(&session_id))
                    };
                    if pending.provider != provider_kind
                        || !pending.email.eq_ignore_ascii_case(&new_account.email)
                    {
                        return Err(anyhow!("OAuth sign-in session does not match this account"));
                    }
                    if let Some(credential) = ready_credential {
                        credential
                    } else if supplied_credential.is_empty() {
                        let (code, callback_state) =
                            oauth::take_callback(&pending)?.ok_or_else(|| {
                                anyhow!(
                                    "OAuth callback is not ready; finish sign-in or paste the code"
                                )
                            })?;
                        oauth::exchange_code(
                            &pending,
                            code.as_str(),
                            callback_state.as_deref().map(|value| value.as_str()),
                        )?
                    } else {
                        let returned_state = returned_state.map(Zeroizing::new);
                        oauth::exchange_code(
                            &pending,
                            supplied_credential.as_str(),
                            returned_state.as_deref().map(|value| value.as_str()),
                        )?
                    }
                } else {
                    if supplied_credential.is_empty() {
                        return Err(anyhow!("account authorization is required"));
                    }
                    supplied_credential
                };

                // Authorization codes and access tokens never enter PersistedState or logs. The
                // resulting provider credential is kept in the OS credential store. OAuth flows
                // retain refresh tokens only inside this same protected entry.
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                if !account_capacity_available(&app.state.accounts, &id) {
                    return Err(anyhow!(
                        "MailGo supports at most {MAX_IMPORTED_ACCOUNTS} accounts"
                    ));
                }
                let previous_accounts = app.state.accounts.clone();
                let previous_folders = app.state.folder_names.clone();
                let mut previous_credentials = vec![(id.clone(), snapshot_credential(&id)?)];
                let commit_result = (|| -> Result<()> {
                    store_credential(&new_account, credential.as_str())?;
                    app.state.accounts.retain(|account| account.id != id);
                    app.state.accounts.push(new_account);
                    app.state.folder_names.remove(&id);
                    app.save()
                })();
                if let Err(error) = commit_result {
                    app.state.accounts = previous_accounts;
                    app.state.folder_names = previous_folders;
                    restore_credentials(&mut previous_credentials);
                    return Err(error);
                }
                clear_credential_snapshots(&mut previous_credentials);
                drop(app);
                if let Err(error) = outbox::resume_account(&cache_dir(), &id) {
                    tracing::warn!(account_id = %id, "could not resume account outbox: {error}");
                }
                Ok(json!({ "id": id, "stored": true }))
            }
            "accounts.import" => {
                let accounts = message
                    .payload
                    .get("accounts")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("accounts must be an array"))?;
                let mut imported_accounts = Vec::new();
                let mut seen_ids = HashSet::new();
                for raw in accounts.iter().take(MAX_IMPORTED_ACCOUNTS) {
                    let id = raw
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim();
                    let provider = raw
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim();
                    let label = raw
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(provider)
                        .trim();
                    let email = raw
                        .get("email")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim();
                    let provider_kind = match providers::ProviderKind::parse(provider) {
                        Ok(provider) => provider,
                        Err(_) => continue,
                    };
                    if !valid_account_id(id)
                        || label.len() > MAX_ACCOUNT_LABEL_LENGTH
                        || providers::validate_email(email).is_err()
                        || !seen_ids.insert(id.to_ascii_lowercase())
                    {
                        continue;
                    }
                    let imported_account = PersistedAccount {
                        id: id.to_string(),
                        provider: provider_kind.as_str().to_string(),
                        label: label.to_string(),
                        email: email.to_string(),
                        unread: 0,
                        accent: "#5f70ee".to_string(),
                        status: "needs-auth".to_string(),
                        last_sync: "等待重新授权".to_string(),
                        imap_host: raw
                            .get("imapHost")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        imap_port: raw
                            .get("imapPort")
                            .and_then(Value::as_u64)
                            .and_then(|value| u16::try_from(value).ok()),
                        imap_security: raw
                            .get("imapSecurity")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        smtp_host: raw
                            .get("smtpHost")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        smtp_port: raw
                            .get("smtpPort")
                            .and_then(Value::as_u64)
                            .and_then(|value| u16::try_from(value).ok()),
                        smtp_security: raw
                            .get("smtpSecurity")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                        authentication: raw
                            .get("authentication")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned),
                    };
                    if profile_for_account(&imported_account).is_ok() {
                        imported_accounts.push(imported_account);
                    }
                }

                let incoming_ids = imported_accounts
                    .iter()
                    .map(|account| account.id.clone())
                    .collect::<HashSet<_>>();
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                if imported_accounts
                    .iter()
                    .any(|account| has_case_variant_account_id(&app.state.accounts, &account.id))
                {
                    return Err(anyhow!(
                        "imported account id differs only by case from an existing account"
                    ));
                }
                if has_existing_account_identity_change(&app.state.accounts, &imported_accounts) {
                    return Err(anyhow!(
                        "imported account identity differs from the existing mailbox"
                    ));
                }
                if has_new_mailbox_identity_conflict(&app.state.accounts, &imported_accounts) {
                    return Err(anyhow!(
                        "one or more imported mailboxes are already configured"
                    ));
                }
                if imported_accounts
                    .iter()
                    .any(|account| app.sync_in_flight.contains(&account.id))
                {
                    return Err(anyhow!(
                        "one or more imported accounts are syncing; retry after synchronization finishes"
                    ));
                }
                if !import_fits_account_capacity(&app.state.accounts, &incoming_ids) {
                    return Err(anyhow!(
                        "importing these accounts would exceed the {MAX_IMPORTED_ACCOUNTS}-account limit"
                    ));
                }
                let mut previous_credentials = Vec::with_capacity(imported_accounts.len());
                for account in &imported_accounts {
                    previous_credentials
                        .push((account.id.clone(), snapshot_credential(&account.id)?));
                }
                let cleanup_result = (|| -> Result<()> {
                    for account in &imported_accounts {
                        sync::remove_account_cache(&cache_dir(), &account.id)?;
                        drafts::remove_account(&cache_dir(), &account.id)?;
                        outbox::remove_account(&cache_dir(), &account.id)?;
                        delete_credential_if_present(&account.id).with_context(|| {
                            format!("remove credential for account {}", account.id)
                        })?;
                    }
                    Ok(())
                })();
                if let Err(error) = cleanup_result {
                    restore_credentials(&mut previous_credentials);
                    return Err(error);
                }

                let previous_accounts = app.state.accounts.clone();
                let previous_folders = app.state.folder_names.clone();
                for imported_account in &imported_accounts {
                    app.state
                        .accounts
                        .retain(|account| account.id != imported_account.id);
                    app.state.accounts.push(imported_account.clone());
                    app.state.folder_names.remove(&imported_account.id);
                }
                if let Err(error) = app.save() {
                    app.state.accounts = previous_accounts;
                    app.state.folder_names = previous_folders;
                    restore_credentials(&mut previous_credentials);
                    return Err(error);
                }
                clear_credential_snapshots(&mut previous_credentials);
                Ok(json!({
                    "imported": imported_accounts.len(),
                    "requiresReauth": true
                }))
            }
            "accounts.remove" => {
                let id = string_field(&message.payload, "id")?;
                if !valid_account_id(&id) {
                    return Err(anyhow!("invalid account id"));
                }
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                if app.sync_in_flight.contains(&id) {
                    return Err(anyhow!(
                        "account sync is in progress; retry account removal after it finishes"
                    ));
                }
                let previous_accounts = app.state.accounts.clone();
                let previous_folders = app.state.folder_names.clone();
                let mut previous_credentials = vec![(id.clone(), snapshot_credential(&id)?)];
                let cleanup_result = (|| -> Result<()> {
                    sync::remove_account_cache(&cache_dir(), &id)?;
                    drafts::remove_account(&cache_dir(), &id)?;
                    outbox::remove_account(&cache_dir(), &id)?;
                    delete_credential_if_present(&id)?;
                    Ok(())
                })();
                if let Err(error) = cleanup_result {
                    restore_credentials(&mut previous_credentials);
                    return Err(error);
                }
                app.state.accounts.retain(|account| account.id != id);
                app.state.folder_names.remove(&id);
                if let Err(error) = app.save() {
                    app.state.accounts = previous_accounts;
                    app.state.folder_names = previous_folders;
                    restore_credentials(&mut previous_credentials);
                    return Err(error);
                }
                clear_credential_snapshots(&mut previous_credentials);
                Ok(json!({ "removed": id }))
            }
            "accounts.diagnose" => {
                ensure_network_allowed(shared)?;
                let account_id = string_field(&message.payload, "accountId")?;
                let account = account_for(shared, &account_id)?;
                let _sync_lease = try_begin_account_sync(shared, &account_id)?;
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let provider = profile.provider;
                let email = account.email.as_str();
                let credential = credential.as_str();
                let incoming_profile = profile.clone();
                let outgoing_profile = profile;
                let (incoming_attempt, outgoing_attempt) = std::thread::scope(|scope| {
                    let incoming = scope.spawn(move || {
                        let started = Instant::now();
                        let result = sync::test_connection(&incoming_profile, email, credential);
                        (started.elapsed(), result)
                    });
                    let outgoing = scope.spawn(move || {
                        let started = Instant::now();
                        let result = send::test_connection(&outgoing_profile, email, credential);
                        (started.elapsed(), result)
                    });
                    let incoming = incoming.join().unwrap_or_else(|_| {
                        (
                            Duration::ZERO,
                            Err(anyhow!("IMAP diagnostic worker stopped")),
                        )
                    });
                    let outgoing = outgoing.join().unwrap_or_else(|_| {
                        (
                            Duration::ZERO,
                            Err(anyhow!("SMTP diagnostic worker stopped")),
                        )
                    });
                    (incoming, outgoing)
                });
                let incoming = connection_diagnostic_channel(
                    &account_id,
                    "imap",
                    incoming_attempt.0,
                    incoming_attempt.1,
                    |error| sync::error_category(error, provider),
                );
                let outgoing = connection_diagnostic_channel(
                    &account_id,
                    "smtp",
                    outgoing_attempt.0,
                    outgoing_attempt.1,
                    send::error_category,
                );
                let ok = incoming.ok && outgoing.ok;
                Ok(json!({
                    "accountId": account_id,
                    "checkedAt": chrono::Utc::now().to_rfc3339(),
                    "ok": ok,
                    "incoming": incoming,
                    "outgoing": outgoing,
                }))
            }
            "accounts.export" => {
                let app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                Ok(json!({
                    "schemaVersion": STATE_SCHEMA_VERSION,
                    "product": "MailGo",
                    "warning": "授权码不会从 Windows Credential Manager 导出。",
                    "accounts": app.state.accounts.iter().map(|account| json!({
                        "id": account.id, "provider": account.provider, "label": account.label, "email": account.email,
                        "imapHost": account.imap_host, "imapPort": account.imap_port, "imapSecurity": account.imap_security,
                        "smtpHost": account.smtp_host, "smtpPort": account.smtp_port, "smtpSecurity": account.smtp_security,
                        "authentication": account.authentication,
                        "status": "requires-reauth", "secretRef": format!("mailgo://{}", account.id),
                    })).collect::<Vec<_>>(),
                }))
            }
            "drafts.list" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                Ok(serde_json::to_value(drafts::list(
                    &cache_dir(),
                    &account_id,
                )?)?)
            }
            "drafts.save" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let (in_reply_to, references) = thread_header_fields(&message.payload)?;
                let draft = drafts::Draft {
                    id: optional_string_field(&message.payload, "id").unwrap_or_default(),
                    account_id,
                    to: text_field(&message.payload, "to", MAX_RECIPIENT_BYTES)?,
                    cc: optional_bounded_string_field(&message.payload, "cc", MAX_RECIPIENT_BYTES)?
                        .unwrap_or_default(),
                    bcc: optional_bounded_string_field(
                        &message.payload,
                        "bcc",
                        MAX_RECIPIENT_BYTES,
                    )?
                    .unwrap_or_default(),
                    subject: text_field(&message.payload, "subject", MAX_SUBJECT_BYTES)?,
                    body: text_field(&message.payload, "body", MAX_MESSAGE_BODY_BYTES)?,
                    html_mode: message
                        .payload
                        .get("htmlMode")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    in_reply_to,
                    references,
                    attachments: Vec::new(),
                    updated_at: 0,
                };
                Ok(serde_json::to_value(drafts::save(&cache_dir(), draft)?)?)
            }
            "drafts.attachment.commit" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let draft_id = string_field(&message.payload, "draftId")?;
                let attachment_id = string_field(&message.payload, "attachmentId")?;
                let upload_id = string_field(&message.payload, "uploadId")?;
                let upload = {
                    let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                    purge_expired_attachment_uploads(&mut app);
                    let upload = app
                        .attachment_uploads
                        .remove(&upload_id)
                        .ok_or_else(|| anyhow!("attachment upload is missing or expired"))?;
                    if upload.bytes.len() != upload.expected_size {
                        app.attachment_uploads.insert(upload_id.clone(), upload);
                        return Err(anyhow!("attachment upload is incomplete"));
                    }
                    upload
                };
                let draft = match drafts::attach(
                    &cache_dir(),
                    &account_id,
                    &draft_id,
                    drafts::NewDraftAttachment {
                        id: attachment_id.clone(),
                        file_name: upload.file_name.clone(),
                        content_type: upload.content_type.clone(),
                        content_id: upload.content_id.clone(),
                    },
                    &upload.bytes,
                ) {
                    Ok(draft) => draft,
                    Err(error) => {
                        if let Ok(mut app) = shared.lock() {
                            app.attachment_uploads.insert(upload_id, upload);
                        }
                        return Err(error);
                    }
                };
                let attachment = draft
                    .attachments
                    .iter()
                    .find(|attachment| attachment.id == attachment_id)
                    .cloned()
                    .ok_or_else(|| anyhow!("committed draft attachment metadata is missing"))?;
                Ok(json!({ "draft": draft, "attachment": attachment }))
            }
            "drafts.attachment.remove" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let draft_id = string_field(&message.payload, "draftId")?;
                let attachment_id = string_field(&message.payload, "attachmentId")?;
                Ok(serde_json::to_value(drafts::remove_attachment(
                    &cache_dir(),
                    &account_id,
                    &draft_id,
                    &attachment_id,
                )?)?)
            }
            "drafts.attachment.start" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let draft_id = string_field(&message.payload, "draftId")?;
                let attachment_id = string_field(&message.payload, "attachmentId")?;
                let attachment =
                    drafts::load_attachment(&cache_dir(), &account_id, &draft_id, &attachment_id)?;
                let size = attachment.bytes.len();
                let download_id = format!("mailgo-draft-attachment-{:016x}", rand::random::<u64>());
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                purge_expired_attachment_downloads(&mut app);
                if app.attachment_downloads.len() >= MAX_ACTIVE_ATTACHMENT_DOWNLOADS {
                    return Err(anyhow!(
                        "too many active attachment downloads; finish or cancel one first"
                    ));
                }
                app.attachment_downloads.insert(
                    download_id.clone(),
                    AttachmentDownloadSession {
                        bytes: attachment.bytes,
                        created_at: Instant::now(),
                    },
                );
                Ok(json!({
                    "downloadId": download_id,
                    "attachmentId": attachment.metadata.id,
                    "fileName": attachment.metadata.file_name,
                    "contentType": attachment.metadata.content_type,
                    "contentId": attachment.metadata.content_id,
                    "size": size,
                    "chunkSize": ATTACHMENT_CHUNK_BYTES,
                }))
            }
            "drafts.remove" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let draft_id = string_field(&message.payload, "id")?;
                Ok(json!({
                    "removed": drafts::remove(&cache_dir(), &account_id, &draft_id)?,
                }))
            }
            "sync.queue_status" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                Ok(serde_json::to_value(sync::pending_mutation_counts(
                    &cache_dir(),
                    &account_id,
                )?)?)
            }
            "mail.outbox.status" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                Ok(serde_json::to_value(outbox::status(
                    &cache_dir(),
                    &account_id,
                )?)?)
            }
            "mail.outbox.retry_all" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let reset = outbox::retry_all(&cache_dir(), &account_id)?;
                Ok(json!({ "accountId": account_id, "reset": reset }))
            }
            "sync.account" => {
                ensure_network_allowed(shared)?;
                let account_id = string_field(&message.payload, "accountId")?;
                let account = account_for(shared, &account_id)?;
                let _sync_lease = try_begin_account_sync(shared, &account_id)?;
                let profile = match profile_for_account(&account) {
                    Ok(profile) => profile,
                    Err(error) => {
                        record_account_sync_failure(
                            shared,
                            &account.id,
                            sync::needs_reauthorization(&error),
                        );
                        return Err(error);
                    }
                };
                let credential = match load_credential(&account) {
                    Ok(credential) => credential,
                    Err(error) => {
                        record_account_sync_failure(
                            shared,
                            &account.id,
                            sync::needs_reauthorization(&error),
                        );
                        return Err(error);
                    }
                };
                if let Err(error) = outbox::flush_due(
                    &cache_dir(),
                    &account.id,
                    profile.clone(),
                    &account.email,
                    &credential,
                ) {
                    tracing::warn!(account_id = %account.id, "manual outbox flush failed: {error}");
                }
                let result = match sync::sync_account(
                    &account.id,
                    profile,
                    &account.email,
                    &credential,
                    &cache_dir(),
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        record_account_sync_failure(
                            shared,
                            &account.id,
                            sync::needs_reauthorization(&error),
                        );
                        return Err(error);
                    }
                };
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                record_account_sync_success(&mut app, &result);
                app.save()?;
                Ok(serde_json::to_value(result)?)
            }
            "sync.page" => {
                ensure_network_allowed(shared)?;
                let account_id = string_field(&message.payload, "accountId")?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let before_uid = message
                    .payload
                    .get("beforeUid")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok());
                let limit = message
                    .payload
                    .get("limit")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(50)
                    .clamp(1, 100);
                let account = account_for(shared, &account_id)?;
                let _sync_lease = try_begin_account_sync(shared, &account_id)?;
                let profile = match profile_for_account(&account) {
                    Ok(profile) => profile,
                    Err(error) => {
                        record_account_sync_failure(
                            shared,
                            &account.id,
                            sync::needs_reauthorization(&error),
                        );
                        return Err(error);
                    }
                };
                let credential = match load_credential(&account) {
                    Ok(credential) => credential,
                    Err(error) => {
                        record_account_sync_failure(
                            shared,
                            &account.id,
                            sync::needs_reauthorization(&error),
                        );
                        return Err(error);
                    }
                };
                let result = match sync::sync_folder_page(
                    &account.id,
                    profile,
                    &account.email,
                    &credential,
                    &folder,
                    before_uid,
                    limit,
                    &cache_dir(),
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        record_account_sync_failure(
                            shared,
                            &account.id,
                            sync::needs_reauthorization(&error),
                        );
                        return Err(error);
                    }
                };
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                record_account_sync_success_with_mode(&mut app, &result, false);
                app.save()?;
                Ok(serde_json::to_value(result)?)
            }
            "mail.search" => {
                ensure_network_allowed(shared)?;
                let query =
                    bounded_string_field(&message.payload, "query", MAX_SEARCH_QUERY_BYTES)?;
                let requested_account_id = optional_string_field(&message.payload, "accountId");
                let limit = message
                    .payload
                    .get("limit")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(MAX_SEARCH_RESULTS)
                    .clamp(1, MAX_SEARCH_RESULTS);
                let accounts = if let Some(account_id) = requested_account_id.as_deref() {
                    vec![account_for(shared, account_id)?]
                } else {
                    shared
                        .lock()
                        .map_err(|_| anyhow!("state lock poisoned"))?
                        .state
                        .accounts
                        .clone()
                };
                let mut messages = Vec::new();
                let mut failed = Vec::new();
                let mut truncated = false;
                for account in accounts {
                    if messages.len() >= limit {
                        truncated = true;
                        break;
                    }
                    let _sync_lease = match try_begin_account_sync(shared, &account.id) {
                        Ok(lease) => lease,
                        Err(error) => {
                            failed.push(json!({
                                "accountId": account.id,
                                "message": error.to_string(),
                            }));
                            continue;
                        }
                    };
                    let remaining = limit.saturating_sub(messages.len());
                    let profile = match profile_for_account(&account) {
                        Ok(profile) => profile,
                        Err(error) => {
                            record_account_sync_failure(
                                shared,
                                &account.id,
                                sync::needs_reauthorization(&error),
                            );
                            failed.push(json!({
                                "accountId": account.id,
                                "message": error.to_string(),
                            }));
                            continue;
                        }
                    };
                    let credential = match load_credential(&account) {
                        Ok(credential) => credential,
                        Err(error) => {
                            let needs_auth = sync::needs_reauthorization(&error);
                            record_account_sync_failure(shared, &account.id, needs_auth);
                            failed.push(json!({
                                "accountId": account.id,
                                "message": if needs_auth { "requires authorization" } else { "credential store unavailable" },
                            }));
                            continue;
                        }
                    };
                    match sync::search_account(
                        &account.id,
                        profile,
                        &account.email,
                        &credential,
                        &query,
                        remaining,
                        &cache_dir(),
                    ) {
                        Ok(result) => {
                            truncated |= result.truncated;
                            messages.extend(result.messages);
                        }
                        Err(error) => {
                            record_account_sync_failure(
                                shared,
                                &account.id,
                                sync::needs_reauthorization(&error),
                            );
                            failed.push(json!({
                                "accountId": account.id,
                                "message": error.to_string(),
                            }));
                        }
                    }
                }
                messages.sort_by(|left, right| right.received_at.cmp(&left.received_at));
                Ok(json!({
                    "messages": messages,
                    "truncated": truncated,
                    "failed": failed,
                }))
            }
            "mail.search.local" => {
                let query =
                    bounded_string_field(&message.payload, "query", MAX_SEARCH_QUERY_BYTES)?;
                let requested_account_id = optional_string_field(&message.payload, "accountId");
                let limit = message
                    .payload
                    .get("limit")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(MAX_SEARCH_RESULTS)
                    .clamp(1, MAX_SEARCH_RESULTS);
                let account_ids = if let Some(account_id) = requested_account_id {
                    vec![account_for(shared, &account_id)?.id]
                } else {
                    shared
                        .lock()
                        .map_err(|_| anyhow!("state lock poisoned"))?
                        .state
                        .accounts
                        .iter()
                        .map(|account| account.id.clone())
                        .collect::<Vec<_>>()
                };
                let result = cache_db::search_messages(&cache_dir(), &account_ids, &query, limit)?;
                Ok(json!({
                    "messages": result.messages,
                    "truncated": result.truncated,
                    "indexing": result.indexing,
                }))
            }
            "sync.all" => {
                ensure_network_allowed(shared)?;
                let accounts = shared
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?
                    .state
                    .accounts
                    .clone();
                tracing::info!(
                    account_count = accounts.len(),
                    "manual all-account sync started"
                );
                let mut synced = Vec::new();
                let mut failed = Vec::new();
                let cache_root = cache_dir();
                let outcomes = sync::map_with_concurrency(
                    &accounts,
                    sync::ACCOUNT_SYNC_CONCURRENCY,
                    |account| run_manual_account_sync(shared, &cache_root, account),
                );
                for outcome in outcomes {
                    match outcome {
                        ManualAccountSyncOutcome::Synced(result) => synced.push(result),
                        ManualAccountSyncOutcome::Failed(error) => failed.push(error),
                    }
                }
                tracing::info!(
                    synced = synced.len(),
                    failed = failed.len(),
                    "manual all-account sync finished"
                );
                Ok(json!({ "accepted": true, "mode": "imap", "synced": synced, "failed": failed }))
            }
            "mail.list" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let before_uid = message
                    .payload
                    .get("beforeUid")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok());
                let limit = message
                    .payload
                    .get("limit")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(48)
                    .clamp(1, 500);
                let page =
                    sync::load_mailbox_page(&cache_dir(), &account_id, &folder, before_uid, limit)?;
                match page {
                    Some(page) => Ok(json!({
                        "offline": true,
                        "mailbox": page.mailbox,
                        "localHasMore": page.local_has_more,
                        "remoteHasMore": page.remote_has_more,
                        "totalCached": page.total_cached,
                        "revision": page.revision,
                    })),
                    None => Ok(json!({ "offline": false, "mailbox": null })),
                }
            }
            "mail.get" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let uid = u32_field(&message.payload, "uid")?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let account = account_for(shared, &account_id)?;
                let cached_message =
                    sync::load_cached_message(&cache_dir(), &account_id, &folder, uid)?;
                if let Some(message) = cached_message.as_ref() {
                    if !message.text_body.is_empty() || message.html_body.is_some() {
                        return Ok(json!({ "offline": true, "message": message }));
                    }
                }
                if offline_mode_enabled(shared)? {
                    return Err(anyhow!(
                        "message body is not cached; turn off offline mode to download it"
                    ));
                }
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let detail = sync::fetch_message(
                    &account.id,
                    profile,
                    &account.email,
                    &credential,
                    &folder,
                    uid,
                    &cache_dir(),
                )?;
                if let Err(error) =
                    sync::save_cached_message(&cache_dir(), &account_id, &detail.message)
                {
                    tracing::warn!(account_id = %account_id, uid, "save full message cache failed: {error}");
                }
                Ok(json!({ "offline": false, "message": detail.message }))
            }
            "mail.attachment.upload.start" => {
                let file_name =
                    valid_upload_file_name(&string_field(&message.payload, "fileName")?)?;
                let content_type = valid_upload_content_type(optional_string_field(
                    &message.payload,
                    "contentType",
                ))?;
                let content_id =
                    valid_upload_content_id(optional_string_field(&message.payload, "contentId"))?;
                let size = message
                    .payload
                    .get("size")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| anyhow!("missing or invalid field: size"))?;
                if size > MAX_OUTGOING_ATTACHMENT_BYTES {
                    return Err(anyhow!(
                        "attachment exceeds the {} MiB per-file limit",
                        MAX_OUTGOING_ATTACHMENT_BYTES / (1024 * 1024)
                    ));
                }
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                purge_expired_attachment_uploads(&mut app);
                if app.attachment_uploads.len() >= MAX_ACTIVE_ATTACHMENT_UPLOADS {
                    return Err(anyhow!(
                        "too many active attachment uploads; finish or cancel one first"
                    ));
                }
                if outgoing_upload_bytes(&app).saturating_add(size)
                    > MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES
                {
                    return Err(anyhow!(
                        "attachments exceed the {} MiB total limit",
                        MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES / (1024 * 1024)
                    ));
                }
                let upload_id = format!("mailgo-upload-{:016x}", rand::random::<u64>());
                app.attachment_uploads.insert(
                    upload_id.clone(),
                    AttachmentUploadSession {
                        file_name,
                        content_type,
                        content_id,
                        expected_size: size,
                        bytes: Vec::with_capacity(size),
                        created_at: Instant::now(),
                    },
                );
                Ok(json!({
                    "uploadId": upload_id,
                    "chunkSize": ATTACHMENT_CHUNK_BYTES,
                    "size": size,
                    "done": size == 0,
                }))
            }
            "mail.attachment.upload.chunk" => {
                let upload_id = string_field(&message.payload, "uploadId")?;
                let offset = message
                    .payload
                    .get("offset")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| anyhow!("missing or invalid field: offset"))?;
                let data_base64 = string_field(&message.payload, "dataBase64")?;
                let max_encoded_chunk = ATTACHMENT_CHUNK_BYTES.div_ceil(3) * 4;
                if data_base64.len() > max_encoded_chunk {
                    return Err(anyhow!("attachment chunk is too large"));
                }
                let bytes = STANDARD
                    .decode(&data_base64)
                    .map_err(|_| anyhow!("attachment chunk is not valid base64"))?;
                if bytes.len() > ATTACHMENT_CHUNK_BYTES {
                    return Err(anyhow!("attachment chunk is too large"));
                }
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                purge_expired_attachment_uploads(&mut app);
                let upload = app
                    .attachment_uploads
                    .get_mut(&upload_id)
                    .ok_or_else(|| anyhow!("attachment upload is missing or expired"))?;
                if offset != upload.bytes.len() {
                    return Err(anyhow!("attachment upload offset is invalid"));
                }
                let next_size = upload.bytes.len().saturating_add(bytes.len());
                if next_size > upload.expected_size {
                    return Err(anyhow!("attachment upload exceeds declared size"));
                }
                if bytes.is_empty() && next_size < upload.expected_size {
                    return Err(anyhow!("empty attachment chunk cannot advance upload"));
                }
                upload.bytes.extend_from_slice(&bytes);
                let done = upload.bytes.len() == upload.expected_size;
                Ok(json!({
                    "uploadId": upload_id,
                    "offset": offset,
                    "nextOffset": upload.bytes.len(),
                    "done": done,
                }))
            }
            "mail.attachment.upload.cancel" => {
                let upload_id = string_field(&message.payload, "uploadId")?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                let cancelled = app.attachment_uploads.remove(&upload_id).is_some();
                Ok(json!({ "uploadId": upload_id, "cancelled": cancelled }))
            }
            "mail.attachment.start" => {
                let account_id = string_field(&message.payload, "accountId")?;
                account_for(shared, &account_id)?;
                let uid = u32_field(&message.payload, "uid")?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let index = message
                    .payload
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| anyhow!("missing or invalid field: index"))?;
                let attachment =
                    sync::load_attachment_data(&cache_dir(), &account_id, &folder, uid, index)?;
                if attachment.bytes.len() > MAX_ATTACHMENT_DOWNLOAD_BYTES {
                    return Err(anyhow!(
                        "attachment exceeds the {} MiB download limit",
                        MAX_ATTACHMENT_DOWNLOAD_BYTES / (1024 * 1024)
                    ));
                }
                let download_id = format!(
                    "mailgo-attachment-{}-{:016x}",
                    account_id,
                    rand::random::<u64>()
                );
                let file_name = attachment.file_name;
                let content_type = attachment.content_type;
                let size = attachment.bytes.len();
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                purge_expired_attachment_downloads(&mut app);
                if app.attachment_downloads.len() >= MAX_ACTIVE_ATTACHMENT_DOWNLOADS {
                    return Err(anyhow!(
                        "too many active attachment downloads; finish or cancel one first"
                    ));
                }
                app.attachment_downloads.insert(
                    download_id.clone(),
                    AttachmentDownloadSession {
                        bytes: attachment.bytes,
                        created_at: Instant::now(),
                    },
                );
                Ok(json!({
                    "downloadId": download_id,
                    "fileName": file_name,
                    "contentType": content_type,
                    "size": size,
                    "chunkSize": ATTACHMENT_CHUNK_BYTES,
                }))
            }
            "mail.attachment.chunk" => {
                let download_id = string_field(&message.payload, "downloadId")?;
                let offset = message
                    .payload
                    .get("offset")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or_default();
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                purge_expired_attachment_downloads(&mut app);
                let (chunk, next_offset, done) = {
                    let download = app
                        .attachment_downloads
                        .get(&download_id)
                        .ok_or_else(|| anyhow!("attachment download is missing or expired"))?;
                    let (next_offset, done) =
                        attachment_chunk_bounds(download.bytes.len(), offset)?;
                    (
                        STANDARD.encode(&download.bytes[offset..next_offset]),
                        next_offset,
                        done,
                    )
                };
                if done {
                    app.attachment_downloads.remove(&download_id);
                }
                Ok(json!({
                    "downloadId": download_id,
                    "offset": offset,
                    "nextOffset": next_offset,
                    "done": done,
                    "dataBase64": chunk,
                }))
            }
            "mail.attachment.cancel" => {
                let download_id = string_field(&message.payload, "downloadId")?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                let cancelled = app.attachment_downloads.remove(&download_id).is_some();
                Ok(json!({ "downloadId": download_id, "cancelled": cancelled }))
            }
            "mail.move" | "mail.archive" | "mail.delete" | "mail.spam" | "mail.inbox" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let uid = u32_field(&message.payload, "uid")?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let target_folder = optional_string_field(&message.payload, "targetFolder");
                let account = account_for(shared, &account_id)?;
                let provider = profile_for_account(&account)?.provider;
                let operation = match message.cmd.as_str() {
                    "mail.move" => "move",
                    "mail.archive" => "archive",
                    "mail.delete" => "delete",
                    "mail.spam" => "move",
                    "mail.inbox" => "move",
                    _ => unreachable!("matched mail mutation command"),
                };
                if matches!(operation, "move" | "archive") && target_folder.is_none() {
                    return Err(anyhow!("{operation} destination is required"));
                }
                if message.cmd == "mail.spam"
                    && !target_folder
                        .as_deref()
                        .is_some_and(|target| sync::is_spam_folder(provider, target))
                {
                    return Err(anyhow!("spam destination must be the provider spam folder"));
                }
                if message.cmd == "mail.inbox"
                    && !target_folder
                        .as_deref()
                        .is_some_and(|target| target.eq_ignore_ascii_case("INBOX"))
                {
                    return Err(anyhow!("inbox destination must be INBOX"));
                }
                if operation == "delete" {
                    let permanent = target_folder
                        .as_deref()
                        .is_none_or(|target| target.eq_ignore_ascii_case(&folder));
                    if permanent && !sync::is_trash_folder(provider, &folder) {
                        return Err(anyhow!(
                            "permanent delete is only allowed from the provider trash folder"
                        ));
                    }
                }
                let offline_mode = offline_mode_enabled(shared)?;
                let result: Result<()> = if offline_mode {
                    match operation {
                        "move" => target_folder
                            .as_deref()
                            .ok_or_else(|| anyhow!("move destination is required"))
                            .and_then(|_| Err(anyhow!("offline-only mode"))),
                        "archive" => target_folder
                            .as_deref()
                            .ok_or_else(|| anyhow!("archive destination is required"))
                            .and_then(|_| Err(anyhow!("offline-only mode"))),
                        "delete" => {
                            let permanent = target_folder
                                .as_deref()
                                .is_none_or(|target| target.eq_ignore_ascii_case(&folder));
                            if permanent && !sync::is_trash_folder(provider, &folder) {
                                Err(anyhow!(
                                    "permanent delete is only allowed from the provider trash folder"
                                ))
                            } else {
                                Err(anyhow!("offline-only mode"))
                            }
                        }
                        _ => unreachable!("matched mail mutation command"),
                    }
                } else {
                    let profile = profile_for_account(&account)?;
                    let credential = load_credential(&account)?;
                    match operation {
                        "move" => target_folder
                            .as_deref()
                            .ok_or_else(|| anyhow!("move destination is required"))
                            .and_then(|target| {
                                sync::move_message(
                                    profile,
                                    &account.email,
                                    &credential,
                                    &folder,
                                    uid,
                                    target,
                                )
                            }),
                        "archive" => target_folder
                            .as_deref()
                            .ok_or_else(|| anyhow!("archive destination is required"))
                            .and_then(|target| {
                                sync::archive_message(
                                    profile,
                                    &account.email,
                                    &credential,
                                    &folder,
                                    uid,
                                    target,
                                )
                            }),
                        "delete" => {
                            let permanent = target_folder
                                .as_deref()
                                .is_none_or(|target| target.eq_ignore_ascii_case(&folder));
                            if permanent && !sync::is_trash_folder(profile.provider, &folder) {
                                Err(anyhow!(
                                    "permanent delete is only allowed from the provider trash folder"
                                ))
                            } else {
                                sync::delete_message(
                                    profile,
                                    &account.email,
                                    &credential,
                                    &folder,
                                    uid,
                                    target_folder.as_deref().unwrap_or(&folder),
                                )
                            }
                        }
                        _ => unreachable!("matched mail mutation command"),
                    }
                };
                let queued = if offline_mode {
                    sync::queue_move_mutation(
                        &cache_dir(),
                        &account_id,
                        operation,
                        &folder,
                        uid,
                        target_folder.as_deref(),
                    )?;
                    true
                } else if let Err(error) = result {
                    if !sync::is_retryable_error(&error, provider) {
                        return Err(error);
                    }
                    sync::queue_move_mutation(
                        &cache_dir(),
                        &account_id,
                        operation,
                        &folder,
                        uid,
                        target_folder.as_deref(),
                    )
                    .with_context(|| {
                        format!("queue mail operation after provider failure: {error}")
                    })?;
                    true
                } else {
                    sync::remove_queued_move(&cache_dir(), &account_id, operation, &folder, uid)?;
                    false
                };
                let cache_result = if operation == "delete"
                    && (target_folder.is_none()
                        || target_folder
                            .as_deref()
                            .is_some_and(|target| target.eq_ignore_ascii_case(&folder)))
                {
                    sync::remove_cached_message(&cache_dir(), &account_id, &folder, uid)
                } else if let Some(target) = target_folder.as_deref() {
                    sync::update_cached_move(&cache_dir(), &account_id, &folder, uid, target)
                } else {
                    Ok(())
                };
                if let Err(error) = cache_result {
                    tracing::warn!(account_id = %account_id, uid, "update local mail operation failed: {error}");
                }
                Ok(json!({
                    "accountId": account_id,
                    "uid": uid,
                    "operation": operation,
                    "queued": queued,
                }))
            }
            "mail.mark_read" | "mail.star" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let uid = u32_field(&message.payload, "uid")?;
                let enabled = message
                    .payload
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let account = account_for(shared, &account_id)?;
                let flag = if message.cmd == "mail.mark_read" {
                    "\\Seen"
                } else {
                    "\\Flagged"
                };
                let queued = if offline_mode_enabled(shared)? {
                    sync::queue_flag_mutation(
                        &cache_dir(),
                        &account_id,
                        &folder,
                        uid,
                        flag,
                        enabled,
                    )?;
                    true
                } else {
                    let profile = profile_for_account(&account)?;
                    let provider = profile.provider;
                    let credential = load_credential(&account)?;
                    match sync::set_flag(
                        profile,
                        &account.email,
                        &credential,
                        &folder,
                        uid,
                        flag,
                        enabled,
                    ) {
                        Ok(()) => {
                            sync::remove_queued_flag(
                                &cache_dir(),
                                &account_id,
                                &folder,
                                uid,
                                flag,
                            )?;
                            false
                        }
                        Err(error) if sync::is_retryable_error(&error, provider) => {
                            sync::queue_flag_mutation(
                                &cache_dir(),
                                &account_id,
                                &folder,
                                uid,
                                flag,
                                enabled,
                            )
                            .with_context(|| {
                                format!("queue mail flag after provider failure: {error}")
                            })?;
                            true
                        }
                        Err(error) => return Err(error),
                    }
                };
                if let Err(error) = sync::update_cached_flags(
                    &cache_dir(),
                    &account_id,
                    &folder,
                    uid,
                    flag,
                    enabled,
                ) {
                    tracing::warn!(account_id = %account_id, uid, "update local message flags failed: {error}");
                }
                Ok(
                    json!({ "accountId": account_id, "uid": uid, "enabled": enabled, "queued": queued }),
                )
            }
            "mail.send" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let (in_reply_to, references) = thread_header_fields(&message.payload)?;
                let to = bounded_string_field(&message.payload, "to", MAX_RECIPIENT_BYTES)?;
                let cc =
                    optional_bounded_string_field(&message.payload, "cc", MAX_RECIPIENT_BYTES)?;
                let bcc =
                    optional_bounded_string_field(&message.payload, "bcc", MAX_RECIPIENT_BYTES)?;
                let subject = bounded_string_field(&message.payload, "subject", MAX_SUBJECT_BYTES)?;
                let text_body =
                    bounded_string_field(&message.payload, "textBody", MAX_MESSAGE_BODY_BYTES)?;
                let html_body = optional_bounded_string_field(
                    &message.payload,
                    "htmlBody",
                    MAX_MESSAGE_BODY_BYTES,
                )?;
                let attachment_ids = match message.payload.get("attachmentIds") {
                    None => Vec::new(),
                    Some(value) => value
                        .as_array()
                        .ok_or_else(|| anyhow!("attachmentIds must be an array"))?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_string)
                                .ok_or_else(|| anyhow!("attachment id must be a string"))
                        })
                        .collect::<Result<Vec<_>>>()?,
                };
                let draft_id = optional_string_field(&message.payload, "draftId");
                let draft_attachment_ids = match message.payload.get("draftAttachmentIds") {
                    None => Vec::new(),
                    Some(value) => value
                        .as_array()
                        .ok_or_else(|| anyhow!("draftAttachmentIds must be an array"))?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_string)
                                .ok_or_else(|| anyhow!("draft attachment id must be a string"))
                        })
                        .collect::<Result<Vec<_>>>()?,
                };
                if attachment_ids
                    .len()
                    .saturating_add(draft_attachment_ids.len())
                    > MAX_OUTGOING_ATTACHMENTS
                {
                    return Err(anyhow!(
                        "a message can contain at most {} attachments",
                        MAX_OUTGOING_ATTACHMENTS
                    ));
                }
                if !draft_attachment_ids.is_empty() && draft_id.is_none() {
                    return Err(anyhow!(
                        "draftId is required when sending persisted draft attachments"
                    ));
                }
                let account = account_for(shared, &account_id)?;
                let mut attachments = Vec::with_capacity(
                    attachment_ids
                        .len()
                        .saturating_add(draft_attachment_ids.len()),
                );
                let mut total = 0usize;
                let mut seen_draft_attachments = HashSet::new();
                for attachment_id in &draft_attachment_ids {
                    if !seen_draft_attachments.insert(attachment_id) {
                        return Err(anyhow!("duplicate draft attachment id"));
                    }
                    let attachment = drafts::load_attachment(
                        &cache_dir(),
                        &account_id,
                        draft_id.as_deref().ok_or_else(|| {
                            anyhow!("draftId is required when sending persisted draft attachments")
                        })?,
                        attachment_id,
                    )?;
                    total = total.saturating_add(attachment.bytes.len());
                    if total > MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES {
                        return Err(anyhow!("attachments exceed the total size limit"));
                    }
                    attachments.push(send::OutgoingAttachment {
                        file_name: attachment.metadata.file_name,
                        content_type: attachment.metadata.content_type,
                        content_id: attachment.metadata.content_id,
                        bytes: attachment.bytes,
                    });
                }
                {
                    let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                    purge_expired_attachment_uploads(&mut app);
                    let mut seen = HashSet::new();
                    for upload_id in &attachment_ids {
                        if !seen.insert(upload_id) {
                            return Err(anyhow!("duplicate attachment upload id"));
                        }
                        let upload = app
                            .attachment_uploads
                            .get(upload_id)
                            .ok_or_else(|| anyhow!("attachment upload is missing or expired"))?;
                        if upload.bytes.len() != upload.expected_size {
                            return Err(anyhow!("attachment upload is incomplete"));
                        }
                        total = total.saturating_add(upload.bytes.len());
                        if total > MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES {
                            return Err(anyhow!("attachments exceed the total size limit"));
                        }
                        attachments.push(send::OutgoingAttachment {
                            file_name: upload.file_name.clone(),
                            content_type: upload.content_type.clone(),
                            content_id: upload.content_id.clone(),
                            bytes: upload.bytes.clone(),
                        });
                    }
                }
                if offline_mode_enabled(shared)? {
                    if let Ok(mut app) = shared.lock() {
                        for upload_id in &attachment_ids {
                            app.attachment_uploads.remove(upload_id);
                        }
                    }
                    let queued = outbox::enqueue(
                        &cache_dir(),
                        outbox::QueuedMessage {
                            id: String::new(),
                            account_id: account_id.clone(),
                            to,
                            cc: cc.unwrap_or_default(),
                            bcc: bcc.unwrap_or_default(),
                            subject,
                            text_body,
                            html_body,
                            in_reply_to,
                            references,
                            attachments: attachments
                                .into_iter()
                                .map(|attachment| outbox::QueuedAttachment {
                                    file_name: attachment.file_name,
                                    content_type: attachment.content_type,
                                    content_id: attachment.content_id,
                                    bytes: attachment.bytes,
                                })
                                .collect(),
                            created_at: 0,
                            updated_at: 0,
                            attempts: 0,
                            next_attempt_at: 0,
                            paused: false,
                            last_error: Some("仅离线模式：联网后将自动发送".to_string()),
                        },
                    )?;
                    return Ok(json!({
                        "sent": false,
                        "queued": true,
                        "outboxId": queued.id,
                        "accountId": account_id,
                    }));
                }
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let outgoing = send::OutgoingMessage {
                    from: &account.email,
                    credential: &credential,
                    to: &to,
                    cc: cc.as_deref(),
                    bcc: bcc.as_deref(),
                    subject: &subject,
                    text_body: &text_body,
                    html_body: html_body.as_deref(),
                    in_reply_to: in_reply_to.as_deref(),
                    references: &references,
                };
                let send_result = send::send_message(profile, &outgoing, &attachments);
                if let Ok(mut app) = shared.lock() {
                    for upload_id in &attachment_ids {
                        app.attachment_uploads.remove(upload_id);
                    }
                }
                match send_result {
                    Ok(()) => Ok(json!({ "sent": true, "queued": false, "accountId": account_id })),
                    Err(error) if send::is_retryable_error(&error) => {
                        let queued = outbox::enqueue(
                            &cache_dir(),
                            outbox::QueuedMessage {
                                id: String::new(),
                                account_id: account_id.clone(),
                                to,
                                cc: cc.unwrap_or_default(),
                                bcc: bcc.unwrap_or_default(),
                                subject,
                                text_body,
                                html_body,
                                in_reply_to,
                                references,
                                attachments: attachments
                                    .into_iter()
                                    .map(|attachment| outbox::QueuedAttachment {
                                        file_name: attachment.file_name,
                                        content_type: attachment.content_type,
                                        content_id: attachment.content_id,
                                        bytes: attachment.bytes,
                                    })
                                    .collect(),
                                created_at: 0,
                                updated_at: 0,
                                attempts: 0,
                                next_attempt_at: 0,
                                paused: false,
                                last_error: None,
                            },
                        )?;
                        Ok(json!({
                            "sent": false,
                            "queued": true,
                            "outboxId": queued.id,
                            "accountId": account_id,
                        }))
                    }
                    Err(error) => Err(error),
                }
            }
            _ => Err(anyhow!("unknown command: {}", message.cmd)),
        }
    })();

    match result {
        Ok(data) => response(&message, true, data),
        Err(error) => response(&message, false, json!({ "message": error.to_string() })),
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn connection_diagnostics_expose_only_bounded_status_and_latency() {
        let successful = connection_diagnostic_channel(
            "account-test",
            "imap",
            Duration::from_millis(47),
            Ok(()),
            |_| "provider",
        );
        assert!(successful.ok);
        assert_eq!(successful.status, "ok");
        assert_eq!(successful.latency_ms, 47);

        let failed = connection_diagnostic_channel(
            "account-test",
            "smtp",
            Duration::from_secs(121),
            Err(anyhow!("diagnostic-sensitive-marker")),
            |_| "authentication",
        );
        assert!(!failed.ok);
        assert_eq!(failed.status, "authentication");
        assert_eq!(failed.latency_ms, 120_000);
        let serialized = serde_json::to_string(&failed).expect("serialize diagnostic result");
        assert!(!serialized.contains("diagnostic-sensitive-marker"));
    }

    #[test]
    fn attachment_chunks_are_bounded_and_resumable() {
        let (next, done) = attachment_chunk_bounds(ATTACHMENT_CHUNK_BYTES + 10, 0).unwrap();
        assert_eq!(next, ATTACHMENT_CHUNK_BYTES);
        assert!(!done);
        let (next, done) = attachment_chunk_bounds(ATTACHMENT_CHUNK_BYTES + 10, next).unwrap();
        assert_eq!(next, ATTACHMENT_CHUNK_BYTES + 10);
        assert!(done);
        assert!(attachment_chunk_bounds(10, 11).is_err());
    }

    #[test]
    fn cancelling_auth_session_discards_ready_credential_and_is_idempotent() {
        let mut app = MailGoState {
            state_path: PathBuf::new(),
            state: PersistedState::default(),
            auth_sessions: HashMap::new(),
            ready_oauth_credentials: HashMap::from([(
                "session-1".to_string(),
                Zeroizing::new("credential-that-must-not-linger".to_string()),
            )]),
            attachment_downloads: HashMap::new(),
            attachment_uploads: HashMap::new(),
            sync_in_flight: HashSet::new(),
            cache_scan: CacheScanState::default(),
        };
        assert!(cancel_auth_session(&mut app, "session-1"));
        assert!(app.ready_oauth_credentials.is_empty());
        assert!(!cancel_auth_session(&mut app, "session-1"));
    }

    #[test]
    fn ipc_text_fields_have_explicit_size_boundaries() {
        let payload = json!({ "subject": "12345" });
        assert!(bounded_string_field(&payload, "subject", 5).is_ok());
        assert!(bounded_string_field(&payload, "subject", 4).is_err());

        let optional = json!({ "htmlBody": "123456" });
        assert!(optional_bounded_string_field(&optional, "htmlBody", 5).is_err());
        assert!(optional_bounded_string_field(&json!({}), "htmlBody", 5)
            .unwrap()
            .is_none());
    }

    #[test]
    fn upload_metadata_rejects_control_characters() {
        assert!(valid_upload_file_name("image\0.png").is_err());
        assert!(valid_upload_content_type(Some("image/png\0".into())).is_err());
        assert!(valid_upload_content_id(Some("inline\r\nid".into())).is_err());
    }

    #[test]
    fn state_decoder_migrates_legacy_fields_and_defaults() {
        let state = decode_persisted_state(
            r#"{
                "accounts": [],
                "theme": "light",
                "minimize_to_tray": false,
                "offline_mode": true,
                "notifications_enabled": false
            }"#,
        )
        .unwrap();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(state.theme, "light");
        assert!(!state.minimize_to_tray);
        assert!(!state.notifications_enabled);
        assert!(!state.remote_images_enabled);
        assert!(!state.hide_ads);
    }

    #[test]
    fn offline_mode_is_opt_in_and_defaults_to_online_sync() {
        assert!(!default_offline_mode());
        assert!(!PersistedState::default().offline_mode);
        let state = decode_persisted_state(r#"{"accounts": []}"#).unwrap();
        assert!(!state.offline_mode);
    }

    #[test]
    fn state_decoder_accepts_camel_case_snapshot_fields() {
        let state = decode_persisted_state(
            r#"{
                "schemaVersion": 1,
                "accounts": [],
                "theme": "dark",
                "minimizeToTray": false,
                "offlineMode": false,
                "notificationsEnabled": true,
                "remoteImagesEnabled": true,
                "hideAds": true,
                "folderNames": {"missing": ["Ignored"]}
            }"#,
        )
        .unwrap();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
        assert!(!state.minimize_to_tray);
        assert!(!state.offline_mode);
        assert!(state.remote_images_enabled);
        assert!(state.hide_ads);
        assert!(state.folder_names.is_empty());
    }

    #[test]
    fn state_decoder_sanitizes_discovered_folder_names() {
        let state = decode_persisted_state(
            r##"{
                "accounts": [{"id":"safe","provider":"qq","label":"first","email":"first@example.invalid","unread":0,"accent":"#111","status":"offline","lastSync":"never"}],
                "folder_names": {
                    "safe": ["Projects", "projects", "Team / 2026", "bad\r\nfolder", ""] ,
                    "missing": ["Should not survive"]
                }
            }"##,
        )
        .unwrap();
        assert_eq!(
            state.folder_names.get("safe").unwrap(),
            &["Projects".to_string(), "Team / 2026".to_string()]
        );
        assert!(!state.folder_names.contains_key("missing"));
    }

    #[test]
    fn state_decoder_rejects_future_schema_versions() {
        let error = decode_persisted_state(
            r#"{
                "schema_version": 99,
                "accounts": []
            }"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("newer than supported"));
    }

    #[test]
    fn account_ids_cannot_escape_the_cache_root() {
        assert!(!valid_account_id("."));
        assert!(!valid_account_id(".."));
        assert!(!valid_account_id("CON"));
        assert!(!valid_account_id("mailbox."));
        assert!(valid_account_id("qq-account-1"));
    }

    #[test]
    fn account_identity_cannot_be_reused_for_another_mailbox() {
        let existing = PersistedAccount {
            id: "account-1".into(),
            provider: "qq".into(),
            label: "QQ".into(),
            email: "person@example.invalid".into(),
            unread: 0,
            accent: "#111".into(),
            status: "offline".into(),
            last_sync: "never".into(),
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            authentication: Some("app-password".into()),
        };
        let mut changed = existing.clone();
        changed.email = "other@example.invalid".into();
        assert!(!account_identity_matches(&existing, &changed));
        changed.email = existing.email.clone();
        changed.label = "New label".into();
        assert!(account_identity_matches(&existing, &changed));

        let mut duplicate = existing.clone();
        duplicate.id = "account-2".into();
        assert!(has_new_mailbox_identity_conflict(
            std::slice::from_ref(&existing),
            std::slice::from_ref(&duplicate)
        ));

        duplicate.email = "another@example.invalid".into();
        assert!(!has_new_mailbox_identity_conflict(
            std::slice::from_ref(&existing),
            std::slice::from_ref(&duplicate)
        ));

        let mut replacement = existing.clone();
        replacement.label = "Replacement".into();
        assert!(!has_new_mailbox_identity_conflict(
            std::slice::from_ref(&existing),
            std::slice::from_ref(&replacement)
        ));
        assert!(!has_existing_account_identity_change(
            std::slice::from_ref(&existing),
            std::slice::from_ref(&replacement)
        ));

        replacement.email = "changed@example.invalid".into();
        assert!(has_existing_account_identity_change(
            std::slice::from_ref(&existing),
            std::slice::from_ref(&replacement)
        ));
    }

    #[test]
    fn custom_mailboxes_with_different_servers_are_distinct() {
        let existing = PersistedAccount {
            id: "custom-1".into(),
            provider: "other".into(),
            label: "Custom".into(),
            email: "person@example.invalid".into(),
            unread: 0,
            accent: "#111".into(),
            status: "offline".into(),
            last_sync: "never".into(),
            imap_host: Some("imap-one.example.invalid".into()),
            imap_port: Some(993),
            imap_security: Some("tls".into()),
            smtp_host: Some("smtp-one.example.invalid".into()),
            smtp_port: Some(465),
            smtp_security: Some("tls".into()),
            authentication: Some("password".into()),
        };
        let mut proposed = existing.clone();
        proposed.id = "custom-2".into();
        proposed.imap_host = Some("imap-two.example.invalid".into());
        assert!(!account_identity_matches(&existing, &proposed));

        proposed.imap_host = existing.imap_host.clone();
        assert!(account_identity_matches(&existing, &proposed));
    }

    #[test]
    fn stored_credentials_are_bound_to_mailbox_identity_and_endpoints() {
        let account = PersistedAccount {
            id: "custom-bound-account".into(),
            provider: "other".into(),
            label: "Custom".into(),
            email: "person@example.invalid".into(),
            unread: 0,
            accent: "#111".into(),
            status: "offline".into(),
            last_sync: "never".into(),
            imap_host: Some("imap.example.invalid".into()),
            imap_port: Some(993),
            imap_security: Some("tls".into()),
            smtp_host: Some("smtp.example.invalid".into()),
            smtp_port: Some(465),
            smtp_security: Some("tls".into()),
            authentication: Some("password".into()),
        };
        let stored = encode_stored_credential(&account, "development-secret")
            .expect("encode bound credential");
        let decoded = decode_stored_credential(&account, stored.as_str())
            .expect("decode bound credential")
            .expect("bound envelope");
        assert_eq!(decoded.as_str(), "development-secret");

        let mut renamed = account.clone();
        renamed.label = "Renamed".into();
        assert!(decode_stored_credential(&renamed, stored.as_str()).is_ok());

        let mut redirected = account.clone();
        redirected.imap_host = Some("attacker.example.invalid".into());
        let error = decode_stored_credential(&redirected, stored.as_str())
            .expect_err("endpoint changes must require reauthorization");
        assert!(error.to_string().contains("reauthorization required"));

        assert!(!legacy_credential_migration_allowed(&account).unwrap());
        let mut qq = account;
        qq.provider = "qq".into();
        assert!(legacy_credential_migration_allowed(&qq).unwrap());
    }

    #[test]
    fn account_ids_are_compared_case_insensitively() {
        let account = PersistedAccount {
            id: "Account-1".into(),
            provider: "qq".into(),
            label: "QQ".into(),
            email: "person@example.invalid".into(),
            unread: 0,
            accent: "#111".into(),
            status: "offline".into(),
            last_sync: "never".into(),
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            authentication: None,
        };
        assert!(has_case_variant_account_id(&[account], "account-1"));
    }

    #[test]
    fn account_sync_lease_prevents_duplicate_work_and_releases_on_drop() {
        let account = PersistedAccount {
            id: "account-1".into(),
            provider: "qq".into(),
            label: "QQ".into(),
            email: "person@example.invalid".into(),
            unread: 0,
            accent: "#111".into(),
            status: "offline".into(),
            last_sync: "never".into(),
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            authentication: None,
        };
        let shared = Arc::new(Mutex::new(MailGoState {
            state_path: PathBuf::new(),
            state: PersistedState {
                accounts: vec![account],
                ..PersistedState::default()
            },
            auth_sessions: HashMap::new(),
            ready_oauth_credentials: HashMap::new(),
            attachment_downloads: HashMap::new(),
            attachment_uploads: HashMap::new(),
            sync_in_flight: HashSet::new(),
            cache_scan: CacheScanState::default(),
        }));
        let lease = try_begin_account_sync(&shared, "account-1").expect("first lease");
        assert!(try_begin_account_sync(&shared, "account-1").is_err());
        drop(lease);
        assert!(try_begin_account_sync(&shared, "account-1").is_ok());
    }

    #[test]
    fn external_url_validation_is_https_only_and_has_no_embedded_credentials() {
        assert!(validate_external_url("https://accounts.example.invalid/settings").is_ok());
        assert!(validate_external_url("http://accounts.example.invalid/settings").is_err());
        assert!(validate_external_url("javascript:alert(1)").is_err());
        assert!(validate_external_url("https://name:token@example.invalid/").is_err());
        assert!(validate_external_url("mailto:person@example.invalid?subject=Hello").is_ok());
        assert!(validate_external_url("mailto:person@example.invalid?body=%0Aunsafe").is_err());
    }

    #[test]
    fn redacted_import_respects_account_capacity_when_replacing_accounts() {
        let existing = (0..MAX_IMPORTED_ACCOUNTS)
            .map(|index| PersistedAccount {
                id: format!("account-{index}"),
                provider: "qq".into(),
                label: format!("Account {index}"),
                email: format!("account-{index}@example.invalid"),
                unread: 0,
                accent: "#5f70ee".into(),
                status: "offline".into(),
                last_sync: "never".into(),
                imap_host: None,
                imap_port: None,
                imap_security: None,
                smtp_host: None,
                smtp_port: None,
                smtp_security: None,
                authentication: None,
            })
            .collect::<Vec<_>>();
        assert!(!import_fits_account_capacity(
            &existing,
            &HashSet::from(["new-account".to_string()])
        ));
        assert!(import_fits_account_capacity(
            &existing,
            &HashSet::from(["account-0".to_string()])
        ));
    }

    #[test]
    fn ipc_capability_validation_rejects_forged_missing_or_malformed_callers() {
        let expected = "A".repeat(IPC_CAPABILITY_LENGTH);
        let forged = format!("{}B", "A".repeat(IPC_CAPABILITY_LENGTH - 1));
        let message = |capability: Option<&str>| IpcMessage {
            id: "capability-test".to_string(),
            cmd: "app.get_state".to_string(),
            payload: capability
                .map(|value| json!({ IPC_CAPABILITY_FIELD: value }))
                .unwrap_or_else(|| json!({})),
        };
        assert!(validate_ipc_capability(&message(Some(&expected)), &forged).is_err());
        assert!(validate_ipc_capability(&message(Some(&expected)), &expected).is_ok());
        assert!(validate_ipc_capability(&message(None), &expected).is_err());
        assert!(validate_ipc_capability(&message(Some(&"A".repeat(47))), &expected).is_err());
        assert!(validate_ipc_capability(
            &message(Some(&format!("{}!", "A".repeat(47)))),
            &expected
        )
        .is_err());
        assert!(validate_ipc_capability(&message(Some(&expected)), "short").is_err());
    }

    #[test]
    fn state_decoder_drops_unsafe_or_duplicate_accounts() {
        let state = decode_persisted_state(
            r##"{
                "accounts": [
                    {"id":"..","provider":"qq","label":"bad","email":"bad@example.invalid","unread":0,"accent":"#111","status":"offline","lastSync":"never"},
                    {"id":"safe","provider":"qq","label":"first","email":"first@example.invalid","unread":0,"accent":"#111","status":"offline","lastSync":"never"},
                    {"id":"safe","provider":"qq","label":"duplicate","email":"second@example.invalid","unread":0,"accent":"#222","status":"offline","lastSync":"never"},
                    {"id":"SAFE","provider":"qq","label":"case duplicate","email":"third@example.invalid","unread":0,"accent":"#222","status":"offline","lastSync":"never"},
                    {"id":"unsafe","provider":"other","label":"bad custom","email":"custom@example.invalid","unread":0,"accent":"#333","status":"offline","lastSync":"never","imapHost":"bad host","imapPort":993,"smtpHost":"smtp.example.invalid","smtpPort":465}
                ]
            }"##,
        )
        .unwrap();
        assert_eq!(state.accounts.len(), 1);
        assert_eq!(state.accounts[0].id, "safe");
        assert_eq!(state.accounts[0].label, "first");
    }
}

fn main() -> Result<()> {
    let _log_guard = init_logging()?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "MailGo starting");
    let Some(_instance_guard) = instance::acquire()? else {
        tracing::info!("existing MailGo instance activated");
        tray::activate_main_window();
        return Ok(());
    };
    let shared_state = Arc::new(Mutex::new(MailGoState::load()?));
    if let Ok(app) = shared_state.lock() {
        tracing::info!(
            account_count = app.state.accounts.len(),
            offline_mode = app.state.offline_mode,
            "local state loaded"
        );
    }
    let state_for_handler = shared_state.clone();
    let ipc_capability = generate_ipc_capability();
    let handler_capability = ipc_capability.clone();
    let handler = FnIpcHandler::new(move |message: IpcMessage| {
        handle_ipc(&state_for_handler, message, &handler_capability)
    });

    let config = AppConfig {
        identifier: "com.neko233.mailgo".to_string(),
        name: "MailGo".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        renderer: RendererConfig {
            webgpu: false,
            trusted_origins: vec!["rdesktop://localhost".to_string()],
            max_ipc_message_bytes: 512 * 1024,
            max_ipc_in_flight: 32,
            ..RendererConfig::default()
        },
        window: WindowConfig {
            title: "MailGo".to_string(),
            width: 1280,
            height: 800,
            min_size: Some((960, 640)),
            decorations: false,
            resizable: true,
            icon: Some(app_window_icon()?),
            ..WindowConfig::default()
        },
        ..AppConfig::default()
    };

    let mut renderer = WebViewRenderer::new(&config)?;
    let dist_root = dist_root()?;
    renderer.set_asset_root(dist_root)?;
    renderer.set_data_directory(webview_data_dir())?;
    renderer.init()?;
    renderer.set_ipc_handler(Box::new(handler));
    let window = renderer.create_window(&config.window)?;
    let app_url = format!("rdesktop://localhost/index.html#ipc={ipc_capability}");
    renderer.load_url(window, &app_url)?;
    let minimize_to_tray = shared_state
        .lock()
        .map_err(|_| anyhow!("state lock poisoned"))?
        .state
        .minimize_to_tray;
    cache_db::spawn_search_indexer(cache_dir());
    cache_db::spawn_encryption_migrator(cache_dir());
    sync::spawn_scheduler(shared_state.clone(), cache_dir());
    tray::start(minimize_to_tray);
    tracing::info!("MailGo desktop window ready; background synchronization scheduled");
    Box::new(renderer).run()?;
    tracing::info!("MailGo stopped");
    Ok(())
}
