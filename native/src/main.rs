use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rdesktop_core::config::{AppConfig, RendererConfig, WindowConfig};
use rdesktop_core::ipc::{FnIpcHandler, IpcMessage, IpcResponse};
use rdesktop_core::renderer::Renderer;
use rdesktop_webview::WebViewRenderer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod classifier;
mod instance;
mod mail;
mod oauth;
mod providers;
mod send;
mod sync;
mod tray;

const APP_SERVICE: &str = "MailGo";
const STATE_SCHEMA_VERSION: u32 = 1;

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
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            accounts: Vec::new(),
            theme: "dark".to_string(),
            minimize_to_tray: true,
            offline_mode: true,
        }
    }
}

struct MailGoState {
    state_path: PathBuf,
    state: PersistedState,
    auth_sessions: HashMap<String, oauth::PendingSession>,
}

impl MailGoState {
    fn load() -> Result<Self> {
        let root = app_data_dir();
        fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        let state_path = root.join("state.json");
        let backup_path = root.join("state.json.bak");
        let state = match fs::read_to_string(&state_path) {
            Ok(contents) => match serde_json::from_str::<PersistedState>(&contents) {
                Ok(state) => state,
                Err(primary_error) => match fs::read_to_string(&backup_path) {
                    Ok(backup) => {
                        serde_json::from_str::<PersistedState>(&backup).with_context(|| {
                            format!("parse {} after {}", backup_path.display(), primary_error)
                        })?
                    }
                    Err(_) => {
                        return Err(primary_error)
                            .with_context(|| format!("parse {}", state_path.display()))
                    }
                },
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::read_to_string(&backup_path) {
                    Ok(backup) => serde_json::from_str::<PersistedState>(&backup)
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

fn optional_u16_field(payload: &Value, name: &str) -> Option<u16> {
    payload
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
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
                app.auth_sessions.insert(session.id.clone(), session);
                Ok(serde_json::to_value(response)?)
            }
            "accounts.add" => {
                let id = string_field(&message.payload, "id")?;
                let provider = string_field(&message.payload, "provider")?;
                let label = string_field(&message.payload, "label")?;
                let email = string_field(&message.payload, "email")?;
                let supplied_credential = string_field(&message.payload, "authorizationCode")?;
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

                let credential = if profile.authentication == providers::Authentication::OAuth2 {
                    let session_id = string_field(&message.payload, "oauthSessionId")?;
                    let returned_state = optional_string_field(&message.payload, "oauthState");
                    let pending = shared
                        .lock()
                        .map_err(|_| anyhow!("state lock poisoned"))?
                        .auth_sessions
                        .remove(&session_id)
                        .ok_or_else(|| anyhow!("OAuth sign-in session is missing or expired"))?;
                    if pending.provider != provider_kind
                        || !pending.email.eq_ignore_ascii_case(&new_account.email)
                    {
                        return Err(anyhow!("OAuth sign-in session does not match this account"));
                    }
                    oauth::exchange_code(&pending, &supplied_credential, returned_state.as_deref())?
                } else {
                    supplied_credential
                };

                // Authorization codes and access tokens never enter PersistedState or logs. The
                // resulting provider credential is kept in the OS credential store. OAuth flows
                // retain refresh tokens only inside this same protected entry.
                credential_entry(&id)?
                    .set_password(&credential)
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
                for raw in accounts {
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
                    if id.is_empty() || provider.is_empty() || !email.contains('@') {
                        continue;
                    }
                    app.state.accounts.retain(|account| account.id != id);
                    app.state.accounts.push(PersistedAccount {
                        id: id.to_string(),
                        provider: provider.to_string(),
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
                if let Some(message) = sync::load_cached_message(&cache_dir(), &account_id, uid)? {
                    if !message.text_body.is_empty() || message.html_body.is_some() {
                        return Ok(json!({ "offline": true, "message": message }));
                    }
                }
                let account = account_for(shared, &account_id)?;
                let profile = profile_for_account(&account)?;
                let credential = load_credential(&account)?;
                let folder = optional_string_field(&message.payload, "folder")
                    .unwrap_or_else(|| "INBOX".to_string());
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
            "mail.attachment" => {
                let account_id = string_field(&message.payload, "accountId")?;
                let uid = u32_field(&message.payload, "uid")?;
                let index = message
                    .payload
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| anyhow!("missing or invalid field: index"))?;
                Ok(serde_json::to_value(sync::load_attachment(
                    &cache_dir(),
                    &account_id,
                    uid,
                    index,
                )?)?)
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
                let to = string_field(&message.payload, "to")?;
                let subject = string_field(&message.payload, "subject")?;
                let text_body = string_field(&message.payload, "textBody")?;
                let html_body = optional_string_field(&message.payload, "htmlBody");
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
                )?;
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
    let dist_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../dist")
        .canonicalize()
        .context("MailGo dist directory is missing; run npm run build first")?;
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
