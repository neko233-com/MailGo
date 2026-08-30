use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rdesktop_core::config::{AppConfig, RendererConfig, WindowConfig};
use rdesktop_core::ipc::{FnIpcHandler, IpcMessage, IpcResponse};
use rdesktop_core::renderer::Renderer;
use rdesktop_webview::WebViewRenderer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zeroize::{Zeroize, Zeroizing};

mod classifier;
mod instance;
mod mail;
mod oauth;
mod providers;
mod send;
mod sync;
mod transfer;
mod tray;

const APP_SERVICE: &str = "MailGo";
const STATE_SCHEMA_VERSION: u32 = 1;
const ATTACHMENT_CHUNK_BYTES: usize = 192 * 1024;
const MAX_ATTACHMENT_DOWNLOAD_BYTES: usize = 256 * 1024 * 1024;
const MAX_LEGACY_ATTACHMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ACTIVE_ATTACHMENT_DOWNLOADS: usize = 2;
const ATTACHMENT_DOWNLOAD_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_OUTGOING_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES: usize = 50 * 1024 * 1024;
const MAX_OUTGOING_ATTACHMENTS: usize = 10;
const MAX_ACTIVE_ATTACHMENT_UPLOADS: usize = 4;
const ATTACHMENT_UPLOAD_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_IMPORTED_ACCOUNTS: usize = 64;
const MAX_ACCOUNT_ID_LENGTH: usize = 128;
const MAX_ACCOUNT_LABEL_LENGTH: usize = 128;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_RECIPIENT_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_MESSAGE_BODY_BYTES: usize = 16 * 1024 * 1024;

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
    theme: String,
    minimize_to_tray: bool,
    offline_mode: bool,
    #[serde(default = "default_notifications_enabled")]
    notifications_enabled: bool,
    #[serde(default)]
    remote_images_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct PersistedStateDisk {
    #[serde(default, alias = "schemaVersion")]
    schema_version: Option<u32>,
    #[serde(default)]
    accounts: Vec<PersistedAccount>,
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
}

fn default_theme() -> String {
    "dark".to_string()
}

fn default_minimize_to_tray() -> bool {
    true
}

fn default_offline_mode() -> bool {
    true
}

fn default_notifications_enabled() -> bool {
    true
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
    Ok(PersistedState {
        schema_version: STATE_SCHEMA_VERSION,
        accounts: disk.accounts,
        theme: if disk.theme == "light" {
            "light".to_string()
        } else {
            "dark".to_string()
        },
        minimize_to_tray: disk.minimize_to_tray,
        offline_mode: disk.offline_mode,
        notifications_enabled: disk.notifications_enabled,
        remote_images_enabled: disk.remote_images_enabled,
    })
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            accounts: Vec::new(),
            theme: "dark".to_string(),
            minimize_to_tray: true,
            offline_mode: true,
            notifications_enabled: true,
            remote_images_enabled: false,
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
}

struct AttachmentDownloadSession {
    bytes: Vec<u8>,
    created_at: Instant,
}

struct AttachmentUploadSession {
    file_name: String,
    content_type: String,
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
        json!({
            "accounts": self.state.accounts,
            "theme": self.state.theme,
            "minimizeToTray": self.state.minimize_to_tray,
            "offlineMode": self.state.offline_mode,
            "notificationsEnabled": self.state.notifications_enabled,
            "remoteImagesEnabled": self.state.remote_images_enabled,
        })
    }
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

fn cache_dir() -> PathBuf {
    app_data_dir().join("cache")
}

fn credential_entry(account_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(APP_SERVICE, account_id)
        .map_err(|error| anyhow!("credential store unavailable: {error}"))
}

fn load_credential(account: &PersistedAccount) -> Result<String> {
    let entry = credential_entry(&account.id)?;
    let raw = entry
        .get_password()
        .map_err(|error| anyhow!("credential unavailable: {error}"))?;
    let provider = providers::ProviderKind::parse(&account.provider)?;
    let refreshed = oauth::refresh_if_needed(provider, &raw)?;
    if refreshed != raw {
        entry
            .set_password(&refreshed)
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

fn optional_u16_field(payload: &Value, name: &str) -> Option<u16> {
    payload
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
}

fn valid_account_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ACCOUNT_ID_LENGTH
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn validate_transfer_account(account: &transfer::TransferAccount) -> Result<()> {
    if !valid_account_id(&account.account.id)
        || account.account.label.len() > MAX_ACCOUNT_LABEL_LENGTH
        || account.credential.is_empty()
        || account.credential.len() > 64 * 1024
    {
        return Err(anyhow!("invalid encrypted account record"));
    }
    providers::ProviderKind::parse(&account.account.provider)?;
    providers::validate_email(&account.account.email)?;
    profile_for_account(&account.account)?;
    Ok(())
}

fn clear_credential_snapshots(previous: &mut [(String, Option<String>)]) {
    for (_, credential) in previous {
        if let Some(value) = credential {
            value.zeroize();
        }
    }
}

fn restore_credentials(previous: &mut [(String, Option<String>)]) {
    for (id, credential) in &mut *previous {
        if let Ok(entry) = credential_entry(id) {
            match credential {
                Some(value) => {
                    let _ = entry.set_password(value);
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
            .any(|character| matches!(character, '\r' | '\n' | '/' | '\\'))
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
            .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err(anyhow!("invalid attachment content type"));
    }
    Ok(content_type)
}

fn purge_expired_auth_sessions(app: &mut MailGoState) {
    app.auth_sessions
        .retain(|_, session| !oauth::is_expired(session));
    let active_sessions = app.auth_sessions.keys().cloned().collect::<HashSet<_>>();
    app.ready_oauth_credentials
        .retain(|session_id, _| active_sessions.contains(session_id));
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

fn handle_ipc(shared: &Arc<Mutex<MailGoState>>, message: IpcMessage) -> IpcResponse {
    let result = (|| -> Result<Value> {
        match message.cmd.as_str() {
            "app.get_state" => Ok(shared
                .lock()
                .map_err(|_| anyhow!("state lock poisoned"))?
                .snapshot()),
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
            "app.hide_window" => {
                tray::hide_main_window();
                Ok(json!({ "hidden": true }))
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
            "auth.start" => {
                let provider =
                    providers::ProviderKind::parse(&string_field(&message.payload, "provider")?)?;
                let email = string_field(&message.payload, "email")?;
                providers::validate_email(&email)?;
                let (session, response) = oauth::start(provider, &email)?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                purge_expired_auth_sessions(&mut app);
                app.auth_sessions.insert(session.id.clone(), session);
                Ok(serde_json::to_value(response)?)
            }
            "auth.device.start" => {
                let provider =
                    providers::ProviderKind::parse(&string_field(&message.payload, "provider")?)?;
                let email = string_field(&message.payload, "email")?;
                providers::validate_email(&email)?;
                let (session, response) = oauth::start_device(provider, &email)?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                purge_expired_auth_sessions(&mut app);
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
                            .insert(session_id.clone(), Zeroizing::new(credential));
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
                let supplied_credential = optional_bounded_string_field(
                    &message.payload,
                    "authorizationCode",
                    MAX_CREDENTIAL_BYTES,
                )?
                .unwrap_or_default();
                let provider_kind = providers::ProviderKind::parse(&provider)?;
                providers::validate_email(&email)?;
                let new_account = PersistedAccount {
                    id: id.clone(),
                    provider: provider_kind.as_str().to_string(),
                    label,
                    email,
                    unread: 0,
                    accent: "#5f70ee".to_string(),
                    status: "synced".to_string(),
                    last_sync: "刚刚同步".to_string(),
                    imap_host: optional_string_field(&message.payload, "imapHost"),
                    imap_port: optional_u16_field(&message.payload, "imapPort"),
                    imap_security: optional_string_field(&message.payload, "imapSecurity"),
                    smtp_host: optional_string_field(&message.payload, "smtpHost"),
                    smtp_port: optional_u16_field(&message.payload, "smtpPort"),
                    smtp_security: optional_string_field(&message.payload, "smtpSecurity"),
                    authentication: optional_string_field(&message.payload, "authentication"),
                };
                let profile = profile_for_account(&new_account)?;

                let oauth_session_id = optional_string_field(&message.payload, "oauthSessionId");
                let credential = if profile.authentication == providers::Authentication::OAuth2
                    && oauth_session_id.is_some()
                {
                    let session_id = oauth_session_id.expect("checked above");
                    let returned_state = optional_string_field(&message.payload, "oauthState");
                    let (pending, ready_credential) = {
                        let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                        purge_expired_auth_sessions(&mut app);
                        let pending = app.auth_sessions.remove(&session_id).ok_or_else(|| {
                            anyhow!("OAuth sign-in session is missing or expired")
                        })?;
                        (pending, app.ready_oauth_credentials.remove(&session_id))
                    };
                    if pending.provider != provider_kind
                        || !pending.email.eq_ignore_ascii_case(&new_account.email)
                    {
                        return Err(anyhow!("OAuth sign-in session does not match this account"));
                    }
                    if let Some(credential) = ready_credential {
                        credential
                    } else {
                        let (code, callback_state) = if supplied_credential.is_empty() {
                            oauth::take_callback(&pending)?.ok_or_else(|| {
                                anyhow!(
                                    "OAuth callback is not ready; finish sign-in or paste the code"
                                )
                            })?
                        } else {
                            (supplied_credential, returned_state)
                        };
                        Zeroizing::new(oauth::exchange_code(
                            &pending,
                            &code,
                            callback_state.as_deref(),
                        )?)
                    }
                } else {
                    if supplied_credential.is_empty() {
                        return Err(anyhow!("account authorization is required"));
                    }
                    Zeroizing::new(supplied_credential)
                };

                // Authorization codes and access tokens never enter PersistedState or logs. The
                // resulting provider credential is kept in the OS credential store. OAuth flows
                // retain refresh tokens only inside this same protected entry.
                credential_entry(&id)?
                    .set_password(credential.as_str())
                    .map_err(|error| anyhow!("save credential: {error}"))?;

                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.accounts.retain(|account| account.id != id);
                app.state.accounts.push(new_account);
                app.save()?;
                Ok(json!({ "id": id, "stored": true }))
            }
            "accounts.import" => {
                let accounts = message
                    .payload
                    .get("accounts")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("accounts must be an array"))?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                let mut imported = 0u32;
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
                    {
                        continue;
                    }
                    if let Ok(entry) = credential_entry(id) {
                        let _ = entry.delete_credential();
                    }
                    app.state.accounts.retain(|account| account.id != id);
                    app.state.accounts.push(PersistedAccount {
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
                    });
                    imported += 1;
                }
                app.save()?;
                Ok(json!({ "imported": imported, "requiresReauth": true }))
            }
            "accounts.remove" => {
                let id = string_field(&message.payload, "id")?;
                if !valid_account_id(&id) {
                    return Err(anyhow!("invalid account id"));
                }
                sync::remove_account_cache(&cache_dir(), &id)?;
                let _ = credential_entry(&id)
                    .and_then(|entry| entry.delete_credential().map_err(anyhow::Error::from));
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.accounts.retain(|account| account.id != id);
                app.save()?;
                Ok(json!({ "removed": id }))
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
            "accounts.export_encrypted" => {
                let passphrase = message
                    .payload
                    .get("passphrase")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("transfer passphrase is required"))?;
                let accounts = shared
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?
                    .state
                    .accounts
                    .clone();
                let mut records = Vec::with_capacity(accounts.len());
                for account in accounts {
                    let credential = load_credential(&account)
                        .context("one or more accounts need reauthorization")?;
                    records.push(transfer::TransferAccount {
                        account,
                        credential,
                    });
                }
                let account_count = records.len();
                let encrypted = transfer::encrypt_accounts(&records, passphrase);
                transfer::clear_credentials(&mut records);
                Ok(json!({
                    "bundle": encrypted?,
                    "accountCount": account_count,
                }))
            }
            "accounts.import_encrypted" => {
                let bundle = string_field(&message.payload, "bundle")?;
                let passphrase = message
                    .payload
                    .get("passphrase")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("transfer passphrase is required"))?;
                let mut records = transfer::decrypt_accounts(&bundle, passphrase)?;
                let import_result = (|| -> Result<u32> {
                    let mut seen = HashSet::new();
                    for record in &records {
                        validate_transfer_account(record)?;
                        if !seen.insert(record.account.id.clone()) {
                            return Err(anyhow!("duplicate account in encrypted bundle"));
                        }
                    }

                    let mut previous = records
                        .iter()
                        .map(|record| {
                            let existing = credential_entry(&record.account.id)
                                .ok()
                                .and_then(|entry| entry.get_password().ok());
                            (record.account.id.clone(), existing)
                        })
                        .collect::<Vec<_>>();
                    for record in &records {
                        if let Err(error) = credential_entry(&record.account.id).and_then(|entry| {
                            entry
                                .set_password(&record.credential)
                                .map_err(anyhow::Error::from)
                        }) {
                            restore_credentials(&mut previous);
                            return Err(error).context("save imported account credential");
                        }
                    }

                    let state_result = (|| -> Result<u32> {
                        for record in &records {
                            sync::remove_account_cache(&cache_dir(), &record.account.id)?;
                        }
                        let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                        for record in &records {
                            let mut account = record.account.clone();
                            account.unread = 0;
                            account.status = "synced".to_string();
                            account.last_sync = "等待首次同步".to_string();
                            app.state.accounts.retain(|item| item.id != account.id);
                            app.state.accounts.push(account);
                        }
                        app.save()?;
                        Ok(records.len() as u32)
                    })();
                    if state_result.is_err() {
                        restore_credentials(&mut previous);
                    } else {
                        clear_credential_snapshots(&mut previous);
                    }
                    state_result
                })();
                transfer::clear_credentials(&mut records);
                Ok(json!({
                    "imported": import_result?,
                    "requiresReauth": false,
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
            "sync.account" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let account = account_for(shared, &account_id)?;
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let result = sync::sync_account(
                    &account.id,
                    profile,
                    &account.email,
                    &credential,
                    &cache_dir(),
                )?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                if let Some(stored) = app
                    .state
                    .accounts
                    .iter_mut()
                    .find(|item| item.id == account.id)
                {
                    stored.unread = result.unread as u32;
                    stored.status = "synced".to_string();
                    stored.last_sync = "刚刚同步".to_string();
                }
                app.save()?;
                Ok(serde_json::to_value(result)?)
            }
            "sync.page" => {
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
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let result = sync::sync_folder_page(
                    &account.id,
                    profile,
                    &account.email,
                    &credential,
                    &folder,
                    before_uid,
                    limit,
                    &cache_dir(),
                )?;
                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                if let Some(stored) = app
                    .state
                    .accounts
                    .iter_mut()
                    .find(|item| item.id == account.id)
                {
                    stored.unread = result.unread as u32;
                    stored.status = "synced".to_string();
                    stored.last_sync = "刚刚同步".to_string();
                }
                app.save()?;
                Ok(serde_json::to_value(result)?)
            }
            "sync.all" => {
                let accounts = shared
                    .lock()
                    .map_err(|_| anyhow!("state lock poisoned"))?
                    .state
                    .accounts
                    .clone();
                let mut synced = Vec::new();
                let mut failed = Vec::new();
                for account in accounts {
                    let profile = match profile_for_account(&account) {
                        Ok(profile) => profile,
                        Err(error) => {
                            failed.push(
                                json!({ "accountId": account.id, "message": error.to_string() }),
                            );
                            continue;
                        }
                    };
                    let credential = match load_credential(&account) {
                        Ok(credential) => credential,
                        Err(error) => {
                            failed.push(json!({ "accountId": account.id, "message": "requires authorization" }));
                            let mut app =
                                shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                            if let Some(stored) = app
                                .state
                                .accounts
                                .iter_mut()
                                .find(|item| item.id == account.id)
                            {
                                stored.status = "needs-auth".to_string();
                                stored.last_sync = "等待重新授权".to_string();
                            }
                            app.save()?;
                            let _ = error;
                            continue;
                        }
                    };
                    match sync::sync_account(
                        &account.id,
                        profile,
                        &account.email,
                        &credential,
                        &cache_dir(),
                    ) {
                        Ok(result) => {
                            let mut app =
                                shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                            if let Some(stored) = app
                                .state
                                .accounts
                                .iter_mut()
                                .find(|item| item.id == account.id)
                            {
                                stored.unread = result.unread as u32;
                                stored.status = "synced".to_string();
                                stored.last_sync = "刚刚同步".to_string();
                            }
                            app.save()?;
                            synced.push(serde_json::to_value(result)?);
                        }
                        Err(error) => failed.push(json!({
                            "accountId": account.id,
                            "message": error.to_string(),
                        })),
                    }
                }
                Ok(json!({ "accepted": true, "mode": "imap", "synced": synced, "failed": failed }))
            }
            "mail.list" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let mailbox = sync::load_mailbox_for_folder(&cache_dir(), &account_id, &folder)?;
                Ok(json!({
                    "offline": mailbox.is_some(),
                    "mailbox": mailbox,
                }))
            }
            "mail.get" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let uid = u32_field(&message.payload, "uid")?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                if let Some(message) =
                    sync::load_cached_message(&cache_dir(), &account_id, &folder, uid)?
                {
                    if !message.text_body.is_empty() || message.html_body.is_some() {
                        return Ok(json!({ "offline": true, "message": message }));
                    }
                }
                let account = account_for(shared, &account_id)?;
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
                let bytes = STANDARD
                    .decode(data_base64)
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
            "mail.attachment" => {
                // Keep the original one-shot command for older clients. New clients use the
                // chunked start/chunk/cancel protocol to cap IPC payload size and support cancel.
                let account_id = string_field(&message.payload, "accountId")?;
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
                if attachment.bytes.len() > MAX_LEGACY_ATTACHMENT_BYTES {
                    return Err(anyhow!(
                        "legacy attachment transfer is limited to {} MiB; use the chunked client",
                        MAX_LEGACY_ATTACHMENT_BYTES / (1024 * 1024)
                    ));
                }
                Ok(json!({
                    "fileName": attachment.file_name,
                    "contentType": attachment.content_type,
                    "dataBase64": STANDARD.encode(attachment.bytes),
                }))
            }
            "mail.move" | "mail.archive" | "mail.delete" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let uid = u32_field(&message.payload, "uid")?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
                let target_folder = optional_string_field(&message.payload, "targetFolder");
                let account = account_for(shared, &account_id)?;
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let operation = match message.cmd.as_str() {
                    "mail.move" => "move",
                    "mail.archive" => "archive",
                    "mail.delete" => "delete",
                    _ => unreachable!("matched mail mutation command"),
                };
                let result = match operation {
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
                    "delete" => sync::delete_message(
                        profile,
                        &account.email,
                        &credential,
                        &folder,
                        uid,
                        target_folder.as_deref().unwrap_or(&folder),
                    ),
                    _ => unreachable!("matched mail mutation command"),
                };
                let queued = if let Err(error) = result {
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
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let flag = if message.cmd == "mail.mark_read" {
                    "\\Seen"
                } else {
                    "\\Flagged"
                };
                let queued = if let Err(error) = sync::set_flag(
                    profile,
                    &account.email,
                    &credential,
                    &folder,
                    uid,
                    flag,
                    enabled,
                ) {
                    sync::queue_flag_mutation(
                        &cache_dir(),
                        &account_id,
                        &folder,
                        uid,
                        flag,
                        enabled,
                    )
                    .with_context(|| format!("queue mail flag after provider failure: {error}"))?;
                    true
                } else {
                    sync::remove_queued_flag(&cache_dir(), &account_id, &folder, uid, flag)?;
                    false
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
                let to = bounded_string_field(&message.payload, "to", MAX_RECIPIENT_BYTES)?;
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
                if attachment_ids.len() > MAX_OUTGOING_ATTACHMENTS {
                    return Err(anyhow!(
                        "a message can contain at most {} attachments",
                        MAX_OUTGOING_ATTACHMENTS
                    ));
                }
                let attachments = {
                    let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                    purge_expired_attachment_uploads(&mut app);
                    let mut seen = HashSet::new();
                    let mut total = 0usize;
                    let mut attachments = Vec::with_capacity(attachment_ids.len());
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
                            bytes: upload.bytes.clone(),
                        });
                    }
                    attachments
                };
                let send_result = (|| -> Result<()> {
                    let account = account_for(shared, &account_id)?;
                    let profile = profile_for_account(&account)?;
                    let credential = load_credential(&account)?;
                    send::send_message(
                        profile,
                        &account.email,
                        &credential,
                        &to,
                        &subject,
                        &text_body,
                        html_body.as_deref(),
                        &attachments,
                    )?;
                    Ok(())
                })();
                if let Ok(mut app) = shared.lock() {
                    for upload_id in &attachment_ids {
                        app.attachment_uploads.remove(upload_id);
                    }
                }
                send_result?;
                Ok(json!({ "sent": true, "accountId": account_id }))
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
mod tests {
    use super::*;

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
                "remoteImagesEnabled": true
            }"#,
        )
        .unwrap();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
        assert!(!state.minimize_to_tray);
        assert!(!state.offline_mode);
        assert!(state.remote_images_enabled);
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
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    let Some(_instance_guard) = instance::acquire()? else {
        tray::activate_main_window();
        return Ok(());
    };
    let shared_state = Arc::new(Mutex::new(MailGoState::load()?));
    let state_for_handler = shared_state.clone();
    let handler =
        FnIpcHandler::new(move |message: IpcMessage| handle_ipc(&state_for_handler, message));

    let config = AppConfig {
        identifier: "com.neko233.mailgo".to_string(),
        name: "MailGo".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        renderer: RendererConfig {
            webgpu: false,
            ..RendererConfig::default()
        },
        window: WindowConfig {
            title: "MailGo".to_string(),
            width: 1440,
            height: 900,
            min_size: Some((1120, 720)),
            decorations: false,
            resizable: true,
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
    renderer.load_url(window, "rdesktop://localhost/index.html")?;
    let minimize_to_tray = shared_state
        .lock()
        .map_err(|_| anyhow!("state lock poisoned"))?
        .state
        .minimize_to_tray;
    sync::spawn_scheduler(shared_state.clone(), cache_dir());
    tray::start(minimize_to_tray);
    Box::new(renderer).run()?;
    Ok(())
}
