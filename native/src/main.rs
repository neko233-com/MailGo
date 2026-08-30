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

const APP_SERVICE: &str = "MailGo";
const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAccount {
    id: String,
    provider: String,
    label: String,
    email: String,
    unread: u32,
    accent: String,
    status: String,
    last_sync: String,
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
}

impl MailGoState {
    fn load() -> Result<Self> {
        let root = app_data_dir();
        fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        let state_path = root.join("state.json");
        let state = match fs::read_to_string(&state_path) {
            Ok(contents) => serde_json::from_str::<PersistedState>(&contents)
                .with_context(|| format!("parse {}", state_path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", state_path.display()))
            }
        };
        Ok(Self { state_path, state })
    }

    fn save(&self) -> Result<()> {
        let temporary_path = self.state_path.with_extension("json.tmp");
        let payload = serde_json::to_vec_pretty(&self.state)?;
        fs::write(&temporary_path, payload)
            .with_context(|| format!("write {}", temporary_path.display()))?;
        if self.state_path.exists() {
            fs::remove_file(&self.state_path)
                .with_context(|| format!("replace {}", self.state_path.display()))?;
        }
        fs::rename(&temporary_path, &self.state_path)
            .with_context(|| format!("commit {}", self.state_path.display()))?;
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

fn credential_entry(account_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(APP_SERVICE, account_id)
        .map_err(|error| anyhow!("credential store unavailable: {error}"))
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
                Ok(json!({ "enabled": enabled }))
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
            "accounts.add" => {
                let id = string_field(&message.payload, "id")?;
                let provider = string_field(&message.payload, "provider")?;
                let label = string_field(&message.payload, "label")?;
                let email = string_field(&message.payload, "email")?;
                let authorization_code = string_field(&message.payload, "authorizationCode")?;
                if !email.contains('@') {
                    return Err(anyhow!("invalid email address"));
                }

                // Authorization codes never enter PersistedState or logs. They are kept in the
                // OS credential store (Windows Credential Manager through keyring-rs).
                credential_entry(&id)?
                    .set_password(&authorization_code)
                    .map_err(|error| anyhow!("save credential: {error}"))?;

                let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
                app.state.accounts.retain(|account| account.id != id);
                app.state.accounts.push(PersistedAccount {
                    id: id.clone(),
                    provider,
                    label,
                    email,
                    unread: 0,
                    accent: "#5f70ee".to_string(),
                    status: "synced".to_string(),
                    last_sync: "刚刚同步".to_string(),
                });
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
                        "status": "requires-reauth", "secretRef": format!("mailgo://{}", account.id),
                    })).collect::<Vec<_>>(),
                }))
            }
            "sync.all" => Ok(json!({ "accepted": true, "mode": "local-first" })),
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
    Box::new(renderer).run()?;
    Ok(())
}
