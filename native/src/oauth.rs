use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::form_urlencoded::Serializer;
use zeroize::{Zeroize, Zeroizing};

use crate::providers::ProviderKind;

const SESSION_TTL_SECONDS: u64 = 10 * 60;
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8765/oauth/callback";
const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const HTTP_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CALLBACK_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const CALLBACK_REQUEST_DEADLINE: Duration = Duration::from_secs(3);
const MAX_PENDING_CALLBACKS: usize = 16;
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;

static HTTP_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

fn http_agent() -> &'static ureq::Agent {
    HTTP_AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(HTTP_CONNECT_TIMEOUT)
            .timeout_read(HTTP_IO_TIMEOUT)
            .timeout_write(HTTP_IO_TIMEOUT)
            .build()
    })
}

struct PendingCallback {
    callback: Arc<Mutex<Option<CallbackResult>>>,
    expires_at: u64,
}

struct LoopbackListener {
    expected_path: String,
    callbacks: Mutex<HashMap<String, PendingCallback>>,
}

static LOOPBACK_LISTENERS: OnceLock<Mutex<HashMap<String, Arc<LoopbackListener>>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub struct PendingSession {
    pub id: String,
    pub provider: ProviderKind,
    pub email: String,
    pub state: String,
    pub code_verifier: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub token_endpoint: String,
    pub created_at: u64,
    pub device_code: Option<String>,
    pub device_expires_at: Option<u64>,
    pub device_interval: u64,
    callback: Arc<Mutex<Option<CallbackResult>>>,
}

impl Drop for PendingSession {
    fn drop(&mut self) {
        self.state.zeroize();
        self.code_verifier.zeroize();
        if let Some(client_secret) = &mut self.client_secret {
            client_secret.zeroize();
        }
        if let Some(device_code) = &mut self.device_code {
            device_code.zeroize();
        }
    }
}

#[derive(Debug, Clone)]
enum CallbackResult {
    Code { code: String, state: Option<String> },
    Error(String),
}

type CallbackCode = (Zeroizing<String>, Option<Zeroizing<String>>);

impl Drop for CallbackResult {
    fn drop(&mut self) {
        match self {
            Self::Code { code, state } => {
                code.zeroize();
                if let Some(state) = state {
                    state.zeroize();
                }
            }
            Self::Error(error) => error.zeroize(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResponse {
    pub session_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub state: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStartResponse {
    pub session_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub message: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone)]
pub enum DevicePollResult {
    Pending { retry_after: u64 },
    Complete { credential: Zeroizing<String> },
}

pub fn is_expired(session: &PendingSession) -> bool {
    let expires_at = session
        .device_expires_at
        .unwrap_or_else(|| session.created_at.saturating_add(SESSION_TTL_SECONDS));
    now_seconds() >= expires_at
}

pub fn cancel(session: &PendingSession) {
    unregister_loopback_callback(&session.redirect_uri, &session.state);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

struct ProviderOAuthConfig {
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: String,
    authorization_endpoint: &'static str,
    token_endpoint: &'static str,
    scopes: &'static str,
    device_authorization_endpoint: Option<&'static str>,
}

pub fn start(provider: ProviderKind, email: &str) -> Result<(PendingSession, StartResponse)> {
    let config = provider_config(provider)?;
    let state = random_string(32);
    let code_verifier = random_string(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let session_id = random_string(24);
    let mut authorization = Serializer::new(format!("{}?", config.authorization_endpoint));
    authorization
        .append_pair("client_id", &config.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("scope", config.scopes)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    if provider == ProviderKind::Google {
        authorization
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
    }
    let authorization_url = authorization.finish();
    let now = now_seconds();
    let callback = Arc::new(Mutex::new(None));
    register_loopback_listener(&config.redirect_uri, &state, callback.clone());
    let session = PendingSession {
        id: session_id.clone(),
        provider,
        email: email.trim().to_string(),
        state: state.clone(),
        code_verifier,
        client_id: config.client_id,
        client_secret: config.client_secret,
        redirect_uri: config.redirect_uri.clone(),
        token_endpoint: config.token_endpoint.to_string(),
        created_at: now,
        callback,
        device_code: None,
        device_expires_at: None,
        device_interval: 0,
    };
    Ok((
        session,
        StartResponse {
            session_id,
            authorization_url,
            redirect_uri: config.redirect_uri,
            state,
            expires_in: SESSION_TTL_SECONDS,
        },
    ))
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenError {
    error: Option<String>,
    error_description: Option<String>,
}

pub fn start_device(
    provider: ProviderKind,
    email: &str,
) -> Result<(PendingSession, DeviceStartResponse)> {
    let config = provider_config(provider)?;
    let endpoint = config
        .device_authorization_endpoint
        .ok_or_else(|| anyhow!("this provider does not expose a device authorization flow"))?;
    let mut form = Serializer::new(String::new());
    form.append_pair("client_id", &config.client_id)
        .append_pair("scope", config.scopes);
    let response = http_agent()
        .post(endpoint)
        .set("Accept", "application/json")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form.finish())
        .map_err(|error| oauth_request_error("OAuth device authorization endpoint", error))?;
    let device: DeviceAuthorizationResponse = response
        .into_json()
        .context("OAuth device response was not valid JSON")?;
    if device.device_code.trim().is_empty() || device.user_code.trim().is_empty() {
        return Err(anyhow!(
            "OAuth device response did not contain a usable code"
        ));
    }
    let expires_in = device.expires_in.max(60);
    let interval = device.interval.unwrap_or(5).max(5);
    let session_id = random_string(24);
    let now = now_seconds();
    let verification_uri = device
        .verification_uri_complete
        .or(device.verification_uri)
        .ok_or_else(|| anyhow!("OAuth device response did not contain a verification URL"))?;
    let session = PendingSession {
        id: session_id.clone(),
        provider,
        email: email.trim().to_string(),
        state: String::new(),
        code_verifier: String::new(),
        client_id: config.client_id,
        client_secret: config.client_secret,
        redirect_uri: String::new(),
        token_endpoint: config.token_endpoint.to_string(),
        created_at: now,
        device_code: Some(device.device_code),
        device_expires_at: Some(now.saturating_add(expires_in)),
        device_interval: interval,
        callback: Arc::new(Mutex::new(None)),
    };
    Ok((
        session,
        DeviceStartResponse {
            session_id,
            user_code: device.user_code,
            verification_uri,
            message: device.message,
            expires_in,
            interval,
        },
    ))
}

pub fn poll_device(session: &PendingSession) -> Result<DevicePollResult> {
    let device_code = session
        .device_code
        .as_deref()
        .ok_or_else(|| anyhow!("OAuth session is not a device flow"))?;
    if session
        .device_expires_at
        .map(|expires_at| now_seconds() >= expires_at)
        .unwrap_or(true)
    {
        return Err(anyhow!("OAuth device authorization expired; start again"));
    }
    let mut form = Serializer::new(String::new());
    form.append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
        .append_pair("device_code", device_code)
        .append_pair("client_id", &session.client_id);
    if let Some(client_secret) = &session.client_secret {
        form.append_pair("client_secret", client_secret);
    }
    let response = match http_agent()
        .post(&session.token_endpoint)
        .set("Accept", "application/json")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form.finish())
    {
        Ok(response) => response,
        Err(ureq::Error::Status(400, response)) => {
            let error = response.into_json::<DeviceTokenError>().ok();
            let error_code = error.as_ref().and_then(|error| error.error.clone());
            let description = error.and_then(|error| error.error_description);
            match error_code {
                Some(error) if error == "authorization_pending" => {
                    return Ok(DevicePollResult::Pending {
                        retry_after: session.device_interval,
                    })
                }
                Some(error) if error == "slow_down" => {
                    return Ok(DevicePollResult::Pending {
                        retry_after: session.device_interval.saturating_add(5),
                    })
                }
                Some(error) if error == "expired_token" => {
                    return Err(anyhow!("OAuth device authorization expired; start again"))
                }
                Some(error) if error == "access_denied" => {
                    return Err(anyhow!("OAuth device authorization was denied"))
                }
                Some(error) => {
                    return Err(anyhow!(
                        "OAuth device authorization failed: {}",
                        description.unwrap_or(error)
                    ))
                }
                None => return Err(anyhow!("OAuth device authorization was rejected")),
            }
        }
        Err(ureq::Error::Status(429, response)) => {
            return Ok(DevicePollResult::Pending {
                retry_after: parse_retry_after(
                    response.header("Retry-After"),
                    session.device_interval.saturating_add(15),
                ),
            })
        }
        Err(ureq::Error::Status(code, _)) => {
            return Err(anyhow!("OAuth device token endpoint returned HTTP {code}"))
        }
        Err(ureq::Error::Transport(_)) => {
            return Err(anyhow!("OAuth device token endpoint is unavailable"))
        }
    };
    let token: TokenResponse = response
        .into_json()
        .context("OAuth device token response was not valid JSON")?;
    Ok(DevicePollResult::Complete {
        credential: serialize_token(token)?,
    })
}

pub fn take_callback(session: &PendingSession) -> Result<Option<CallbackCode>> {
    let mut callback = session
        .callback
        .lock()
        .map_err(|_| anyhow!("OAuth callback state is unavailable"))?;
    let Some(result) = callback.take() else {
        return Ok(None);
    };
    let mut result = result;
    match &mut result {
        CallbackResult::Code { code, state } => Ok(Some((
            Zeroizing::new(std::mem::take(code)),
            state.take().map(Zeroizing::new),
        ))),
        CallbackResult::Error(error) => Err(anyhow!(
            "OAuth provider returned an error: {}",
            std::mem::take(error)
        )),
    }
}

pub fn exchange_code(
    session: &PendingSession,
    code: &str,
    returned_state: Option<&str>,
) -> Result<Zeroizing<String>> {
    if now_seconds().saturating_sub(session.created_at) > SESSION_TTL_SECONDS {
        return Err(anyhow!("OAuth sign-in session expired; start again"));
    }
    let returned_state = returned_state.ok_or_else(|| anyhow!("OAuth state is missing"))?;
    if returned_state != session.state {
        return Err(anyhow!("OAuth state validation failed"));
    }
    let code = code.trim();
    if code.is_empty() || code.len() > 8192 {
        return Err(anyhow!("invalid OAuth authorization code"));
    }
    let mut form = Serializer::new(String::new());
    form.append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("client_id", &session.client_id)
        .append_pair("redirect_uri", &session.redirect_uri)
        .append_pair("code_verifier", &session.code_verifier);
    if let Some(client_secret) = &session.client_secret {
        form.append_pair("client_secret", client_secret);
    }
    let response = http_agent()
        .post(&session.token_endpoint)
        .set("Accept", "application/json")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form.finish())
        .map_err(|error| oauth_request_error("OAuth token endpoint", error))?;
    let token: TokenResponse = response
        .into_json()
        .context("OAuth token response was not valid JSON")?;
    if token.access_token.trim().is_empty() {
        return Err(anyhow!(
            "OAuth token response did not contain an access token"
        ));
    }
    serialize_token(token)
}

fn serialize_token(token: TokenResponse) -> Result<Zeroizing<String>> {
    if token.access_token.trim().is_empty() {
        return Err(anyhow!(
            "OAuth token response did not contain an access token"
        ));
    }
    let expires_at = token
        .expires_in
        .map(|seconds| now_seconds().saturating_add(seconds));
    Ok(Zeroizing::new(
        serde_json::to_string(&StoredCredential {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            token_type: token.token_type.unwrap_or_else(|| "Bearer".into()),
            expires_at,
        })
        .context("serialize OAuth credential")?,
    ))
}

pub fn access_token(raw: &str) -> String {
    serde_json::from_str::<StoredCredential>(raw)
        .map(|credential| credential.access_token)
        .unwrap_or_else(|_| raw.to_string())
}

pub fn refresh_if_needed(provider: ProviderKind, raw: &str) -> Result<Zeroizing<String>> {
    let Ok(stored) = serde_json::from_str::<StoredCredential>(raw) else {
        return Ok(Zeroizing::new(raw.to_string()));
    };
    let should_refresh = stored
        .expires_at
        .map(|expires_at| expires_at <= now_seconds().saturating_add(60))
        .unwrap_or(false);
    if !should_refresh {
        return Ok(Zeroizing::new(raw.to_string()));
    }
    let Some(refresh_token) = stored.refresh_token.as_deref() else {
        return Err(anyhow!(
            "OAuth access token expired; reauthorization is required"
        ));
    };
    let config = provider_config(provider)?;
    let mut form = Serializer::new(String::new());
    form.append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .append_pair("client_id", &config.client_id);
    if let Some(client_secret) = &config.client_secret {
        form.append_pair("client_secret", client_secret);
    }
    let response = http_agent()
        .post(config.token_endpoint)
        .set("Accept", "application/json")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form.finish())
        .map_err(|error| oauth_request_error("OAuth refresh endpoint", error))?;
    let token: TokenResponse = response
        .into_json()
        .context("OAuth refresh response was not valid JSON")?;
    if token.access_token.trim().is_empty() {
        return Err(anyhow!(
            "OAuth refresh response did not contain an access token"
        ));
    }
    Ok(Zeroizing::new(
        serde_json::to_string(&StoredCredential {
            access_token: token.access_token,
            refresh_token: token.refresh_token.or(stored.refresh_token),
            token_type: token.token_type.unwrap_or(stored.token_type),
            expires_at: token
                .expires_in
                .map(|seconds| now_seconds().saturating_add(seconds)),
        })
        .context("serialize refreshed OAuth credential")?,
    ))
}

fn provider_config(provider: ProviderKind) -> Result<ProviderOAuthConfig> {
    match provider {
        ProviderKind::Google => Ok(ProviderOAuthConfig {
            client_id: required_env("MAILGO_GOOGLE_CLIENT_ID")?,
            client_secret: optional_env("MAILGO_GOOGLE_CLIENT_SECRET"),
            redirect_uri: configured_redirect_uri("MAILGO_GOOGLE_REDIRECT_URI")?,
            authorization_endpoint: "https://accounts.google.com/o/oauth2/v2/auth",
            token_endpoint: "https://oauth2.googleapis.com/token",
            scopes: "openid email https://mail.google.com/",
            device_authorization_endpoint: None,
        }),
        ProviderKind::Outlook => Ok(ProviderOAuthConfig {
            client_id: required_env("MAILGO_OUTLOOK_CLIENT_ID")?,
            client_secret: optional_env("MAILGO_OUTLOOK_CLIENT_SECRET"),
            redirect_uri: configured_redirect_uri("MAILGO_OUTLOOK_REDIRECT_URI")?,
            authorization_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            scopes: "openid email offline_access https://outlook.office365.com/IMAP.AccessAsUser.All https://outlook.office365.com/SMTP.Send",
            device_authorization_endpoint: Some("https://login.microsoftonline.com/common/oauth2/v2.0/devicecode"),
        }),
        _ => Err(anyhow!("this provider does not expose a MailGo OAuth flow")),
    }
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("OAuth client is not configured; set {name}"))
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate_loopback_redirect_uri(value: &str) -> Result<String> {
    let value = value.trim();
    if value.len() > 2048 {
        return Err(anyhow!("OAuth redirect URI is too long"));
    }
    let parsed = url::Url::parse(value).context("OAuth redirect URI is invalid")?;
    if parsed.scheme() != "http"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.port().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path().is_empty()
        || !parsed.path().starts_with('/')
    {
        return Err(anyhow!(
            "OAuth redirect URI must be an http URL on 127.0.0.1 with an explicit port and path"
        ));
    }
    Ok(value.to_string())
}

fn configured_redirect_uri(name: &str) -> Result<String> {
    let value = optional_env(name).unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string());
    validate_loopback_redirect_uri(&value)
}

fn parse_retry_after(value: Option<&str>, fallback: u64) -> u64 {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.clamp(5, 3600))
        .unwrap_or(fallback.clamp(5, 3600))
}

fn oauth_request_error(context: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(429, response) => {
            let retry_after = response
                .header("Retry-After")
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(|seconds| seconds.clamp(5, 3600));
            match retry_after {
                Some(seconds) => {
                    anyhow!("{context} is rate limited (HTTP 429; retry after {seconds} seconds)")
                }
                None => anyhow!("{context} is rate limited (HTTP 429)"),
            }
        }
        ureq::Error::Status(code, _) => anyhow!("{context} returned HTTP {code}"),
        ureq::Error::Transport(_) => anyhow!("{context} is unavailable"),
    }
}

fn register_loopback_listener(
    redirect_uri: &str,
    state: &str,
    callback: Arc<Mutex<Option<CallbackResult>>>,
) {
    let Ok(parsed) = url::Url::parse(redirect_uri) else {
        return;
    };
    if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
        return;
    }
    let Some(port) = parsed.port_or_known_default() else {
        return;
    };
    let expected_path = parsed.path().to_string();
    let listener_key = format!("{port}:{expected_path}");
    let listeners = LOOPBACK_LISTENERS.get_or_init(|| Mutex::new(HashMap::new()));
    let listener_state = {
        let Ok(mut listeners) = listeners.lock() else {
            return;
        };
        if let Some(existing) = listeners.get(&listener_key) {
            existing.clone()
        } else {
            let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
                tracing::debug!(
                    port,
                    "OAuth loopback port unavailable; manual code entry remains enabled"
                );
                return;
            };
            let created = Arc::new(LoopbackListener {
                expected_path: expected_path.clone(),
                callbacks: Mutex::new(HashMap::new()),
            });
            let thread_state = created.clone();
            if thread::Builder::new()
                .name("mailgo-oauth-callback".into())
                .spawn(move || run_loopback_listener(listener, thread_state))
                .is_err()
            {
                tracing::debug!(port, "OAuth loopback listener thread could not start");
                return;
            }
            listeners.insert(listener_key, created.clone());
            created
        }
    };

    let Ok(mut callbacks) = listener_state.callbacks.lock() else {
        return;
    };
    let now = now_seconds();
    callbacks.retain(|_, pending| pending.expires_at > now);
    if callbacks.len() >= MAX_PENDING_CALLBACKS {
        tracing::debug!(
            port,
            "OAuth callback capacity reached; manual code entry remains enabled"
        );
        return;
    }
    callbacks.insert(
        state.to_string(),
        PendingCallback {
            callback,
            expires_at: now.saturating_add(SESSION_TTL_SECONDS),
        },
    );
}

fn unregister_loopback_callback(redirect_uri: &str, state: &str) {
    if redirect_uri.is_empty() || state.is_empty() {
        return;
    }
    let Ok(parsed) = url::Url::parse(redirect_uri) else {
        return;
    };
    if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
        return;
    }
    let Some(port) = parsed.port_or_known_default() else {
        return;
    };
    let listener_key = format!("{port}:{}", parsed.path());
    let Some(listeners) = LOOPBACK_LISTENERS.get() else {
        return;
    };
    let Ok(listeners) = listeners.lock() else {
        return;
    };
    let listener = listeners.get(&listener_key).cloned();
    drop(listeners);
    let Some(listener) = listener else {
        return;
    };
    let Ok(mut callbacks) = listener.callbacks.lock() else {
        return;
    };
    callbacks.remove(state);
}

fn run_loopback_listener(listener: TcpListener, state: Arc<LoopbackListener>) {
    loop {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(CALLBACK_SOCKET_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CALLBACK_SOCKET_TIMEOUT));
        let Some(request_target) = read_request_target(&mut stream) else {
            continue;
        };
        let callback_result = parse_callback(&request_target, &state.expected_path);
        let callback = is_expected_callback_path(&request_target, &state.expected_path)
            .then(|| callback_state(&request_target))
            .flatten()
            .and_then(|callback_state| {
                state
                    .callbacks
                    .lock()
                    .ok()?
                    .remove(&callback_state)
                    .map(|pending| pending.callback)
            });
        let body = if callback.is_some() {
            match &callback_result {
                CallbackResult::Code { .. } => {
                    "<h1>MailGo 授权完成</h1><p>可以返回 MailGo 继续同步。</p>"
                }
                CallbackResult::Error(_) => "<h1>MailGo 授权失败</h1><p>可以返回 MailGo 重试。</p>",
            }
        } else {
            "<h1>MailGo 授权请求已失效</h1><p>请返回 MailGo 重新开始授权。</p>"
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        let _ = stream.write_all(response.as_bytes());
        if let Some(callback) = callback {
            if let Ok(mut stored) = callback.lock() {
                *stored = Some(callback_result);
            }
        }
    }
}

fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    read_request_target_until(stream, Instant::now() + CALLBACK_REQUEST_DEADLINE)
}

fn read_request_target_until(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        stream
            .set_read_timeout(Some(remaining.min(CALLBACK_SOCKET_TIMEOUT)))
            .ok()?;
        let size = stream.read(&mut chunk).ok()?;
        if size == 0 {
            break;
        }
        if buffer.len().saturating_add(size) > MAX_CALLBACK_REQUEST_BYTES {
            return None;
        }
        buffer.extend_from_slice(&chunk[..size]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&buffer).ok()?;
    let mut fields = request.lines().next()?.split_whitespace();
    let method = fields.next()?;
    let target = fields.next()?;
    if method != "GET" || !target.starts_with('/') || target.len() > 8192 {
        return None;
    }
    Some(target.to_string())
}

fn callback_state(target: &str) -> Option<String> {
    let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    let state = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))?;
    if state.is_empty() || state.len() > 256 {
        return None;
    }
    Some(state)
}

fn is_expected_callback_path(target: &str, expected_path: &str) -> bool {
    url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map(|parsed| parsed.path() == expected_path)
        .unwrap_or(false)
}

fn parse_callback(target: &str, expected_path: &str) -> CallbackResult {
    let Ok(parsed) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return CallbackResult::Error("invalid callback URL".into());
    };
    if parsed.path() != expected_path {
        return CallbackResult::Error("unexpected callback path".into());
    }
    let query = parsed
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    if let Some(error) = query.get("error") {
        return CallbackResult::Error(error.chars().take(120).collect());
    }
    let Some(code) = query.get("code") else {
        return CallbackResult::Error("authorization code is missing".into());
    };
    if code.is_empty() || code.len() > 8192 {
        return CallbackResult::Error("authorization code is invalid".into());
    }
    let Some(state) = query.get("state") else {
        return CallbackResult::Error("OAuth state is missing".into());
    };
    CallbackResult::Code {
        code: code.to_string(),
        state: Some(state.to_string()),
    }
}

fn random_string(length: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
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

    #[test]
    fn pkce_start_contains_no_secret_or_token() {
        std::env::set_var("MAILGO_GOOGLE_CLIENT_ID", "desktop-client-id");
        let (session, response) = start(ProviderKind::Google, "person@example.com").unwrap();
        assert!(response.authorization_url.contains("code_challenge="));
        assert!(response.authorization_url.contains("state="));
        assert!(response.authorization_url.contains("access_type=offline"));
        assert!(!response.authorization_url.contains("access_token"));
        assert_eq!(session.email, "person@example.com");
    }

    #[test]
    fn legacy_passwords_remain_supported() {
        assert_eq!(access_token("app-password"), "app-password");
    }

    #[test]
    fn callback_requires_matching_path_and_state() {
        assert!(matches!(
            parse_callback("/oauth/callback?code=abc&state=xyz", "/oauth/callback"),
            CallbackResult::Code { .. }
        ));
        assert!(matches!(
            parse_callback("/oauth/callback?code=abc", "/oauth/callback"),
            CallbackResult::Error(_)
        ));
        assert!(matches!(
            parse_callback("/wrong?code=abc&state=xyz", "/oauth/callback"),
            CallbackResult::Error(_)
        ));
    }

    #[test]
    fn callback_state_is_bounded_and_extracted_for_dispatch() {
        assert_eq!(
            callback_state("/oauth/callback?code=abc&state=xyz"),
            Some("xyz".to_string())
        );
        assert!(callback_state(&format!("/oauth/callback?state={}", "x".repeat(257))).is_none());
        assert!(callback_state("/oauth/callback?code=abc").is_none());
        assert!(is_expected_callback_path(
            "/oauth/callback?code=abc&state=xyz",
            "/oauth/callback"
        ));
        assert!(!is_expected_callback_path(
            "/wrong?code=abc&state=xyz",
            "/oauth/callback"
        ));
    }

    #[test]
    fn retry_after_is_bounded_and_has_a_safe_fallback() {
        assert_eq!(parse_retry_after(Some("12"), 5), 12);
        assert_eq!(parse_retry_after(Some("1"), 5), 5);
        assert_eq!(parse_retry_after(Some("99999"), 5), 3600);
        assert_eq!(parse_retry_after(Some("invalid"), 15), 15);
    }

    #[test]
    fn redirect_uri_is_restricted_to_loopback_http() {
        assert!(validate_loopback_redirect_uri("http://127.0.0.1:8765/oauth/callback").is_ok());
        assert!(
            validate_loopback_redirect_uri("http://127.0.0.1:8765/oauth/callback?x=1").is_err()
        );
        assert!(validate_loopback_redirect_uri("https://127.0.0.1:8765/oauth/callback").is_err());
        assert!(validate_loopback_redirect_uri("http://127.0.0.1/oauth/callback").is_err());
        assert!(validate_loopback_redirect_uri("http://example.com:8765/oauth/callback").is_err());
        assert!(
            validate_loopback_redirect_uri("http://user:pass@127.0.0.1:8765/oauth/callback")
                .is_err()
        );
    }

    #[test]
    fn request_target_handles_split_http_headers_with_a_bound() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("callback fixture listener");
        let address = listener.local_addr().expect("callback fixture address");
        let writer = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("callback fixture connection");
            stream
                .write_all(b"GET /oauth/call")
                .expect("first request chunk");
            stream
                .write_all(b"back?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\nignored")
                .expect("second request chunk");
        });
        let (mut stream, _) = listener.accept().expect("callback fixture accept");
        stream
            .set_read_timeout(Some(CALLBACK_SOCKET_TIMEOUT))
            .expect("callback fixture timeout");
        assert_eq!(
            read_request_target(&mut stream).as_deref(),
            Some("/oauth/callback?code=abc&state=xyz")
        );
        writer.join().expect("callback fixture writer");
    }

    #[test]
    fn request_target_has_one_absolute_deadline_for_trickle_clients() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("callback fixture listener");
        let address = listener.local_addr().expect("callback fixture address");
        let writer = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("callback fixture connection");
            for _ in 0..10 {
                if stream.write_all(b"G").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(35));
            }
        });
        let (mut stream, _) = listener.accept().expect("callback fixture accept");
        let started = Instant::now();
        assert!(
            read_request_target_until(&mut stream, started + Duration::from_millis(120)).is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        writer.join().expect("callback fixture writer");
    }
}
