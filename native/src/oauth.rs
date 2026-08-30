use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::form_urlencoded::Serializer;

use crate::providers::ProviderKind;

const SESSION_TTL_SECONDS: u64 = 10 * 60;
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8765/oauth/callback";

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
    callback: Arc<Mutex<Option<CallbackResult>>>,
}

#[derive(Debug, Clone)]
enum CallbackResult {
    Code { code: String, state: Option<String> },
    Error(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartResponse {
    pub session_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub expires_in: u64,
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
}

pub fn start(provider: ProviderKind, email: &str) -> Result<(PendingSession, StartResponse)> {
    let config = provider_config(provider)?;
    let state = random_string(32);
    let code_verifier = random_string(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let session_id = random_string(24);
    let authorization_url = Serializer::new(format!("{}?", config.authorization_endpoint))
        .append_pair("client_id", &config.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("scope", config.scopes)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .finish();
    let now = now_seconds();
    let callback = Arc::new(Mutex::new(None));
    spawn_loopback_listener(&config.redirect_uri, callback.clone());
    let session = PendingSession {
        id: session_id.clone(),
        provider,
        email: email.trim().to_string(),
        state,
        code_verifier,
        client_id: config.client_id,
        client_secret: config.client_secret,
        redirect_uri: config.redirect_uri.clone(),
        token_endpoint: config.token_endpoint.to_string(),
        created_at: now,
        callback,
    };
    Ok((
        session,
        StartResponse {
            session_id,
            authorization_url,
            redirect_uri: config.redirect_uri,
            expires_in: SESSION_TTL_SECONDS,
        },
    ))
}

pub fn take_callback(session: &PendingSession) -> Result<Option<(String, Option<String>)>> {
    let mut callback = session
        .callback
        .lock()
        .map_err(|_| anyhow!("OAuth callback state is unavailable"))?;
    let Some(result) = callback.take() else {
        return Ok(None);
    };
    match result {
        CallbackResult::Code { code, state } => Ok(Some((code, state))),
        CallbackResult::Error(error) => Err(anyhow!("OAuth provider returned an error: {error}")),
    }
}

pub fn exchange_code(
    session: &PendingSession,
    code: &str,
    returned_state: Option<&str>,
) -> Result<String> {
    if now_seconds().saturating_sub(session.created_at) > SESSION_TTL_SECONDS {
        return Err(anyhow!("OAuth sign-in session expired; start again"));
    }
    if let Some(returned_state) = returned_state {
        if returned_state != session.state {
            return Err(anyhow!("OAuth state validation failed"));
        }
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
    let response = ureq::post(&session.token_endpoint)
        .set("Accept", "application/json")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form.finish())
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => anyhow!("OAuth token endpoint returned HTTP {code}"),
            ureq::Error::Transport(_) => anyhow!("OAuth token endpoint is unavailable"),
        })?;
    let token: TokenResponse = response
        .into_json()
        .context("OAuth token response was not valid JSON")?;
    if token.access_token.trim().is_empty() {
        return Err(anyhow!(
            "OAuth token response did not contain an access token"
        ));
    }
    let expires_at = token
        .expires_in
        .map(|seconds| now_seconds().saturating_add(seconds));
    let credential = StoredCredential {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        token_type: token.token_type.unwrap_or_else(|| "Bearer".into()),
        expires_at,
    };
    serde_json::to_string(&credential).context("serialize OAuth credential")
}

pub fn access_token(raw: &str) -> String {
    serde_json::from_str::<StoredCredential>(raw)
        .map(|credential| credential.access_token)
        .unwrap_or_else(|_| raw.to_string())
}

pub fn refresh_if_needed(provider: ProviderKind, raw: &str) -> Result<String> {
    let Ok(stored) = serde_json::from_str::<StoredCredential>(raw) else {
        return Ok(raw.to_string());
    };
    let should_refresh = stored
        .expires_at
        .map(|expires_at| expires_at <= now_seconds().saturating_add(60))
        .unwrap_or(false);
    if !should_refresh {
        return Ok(raw.to_string());
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
    let response = ureq::post(config.token_endpoint)
        .set("Accept", "application/json")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form.finish())
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => anyhow!("OAuth refresh endpoint returned HTTP {code}"),
            ureq::Error::Transport(_) => anyhow!("OAuth refresh endpoint is unavailable"),
        })?;
    let token: TokenResponse = response
        .into_json()
        .context("OAuth refresh response was not valid JSON")?;
    if token.access_token.trim().is_empty() {
        return Err(anyhow!(
            "OAuth refresh response did not contain an access token"
        ));
    }
    serde_json::to_string(&StoredCredential {
        access_token: token.access_token,
        refresh_token: token.refresh_token.or(stored.refresh_token),
        token_type: token.token_type.unwrap_or(stored.token_type),
        expires_at: token
            .expires_in
            .map(|seconds| now_seconds().saturating_add(seconds)),
    })
    .context("serialize refreshed OAuth credential")
}

fn provider_config(provider: ProviderKind) -> Result<ProviderOAuthConfig> {
    match provider {
        ProviderKind::Google => Ok(ProviderOAuthConfig {
            client_id: required_env("MAILGO_GOOGLE_CLIENT_ID")?,
            client_secret: optional_env("MAILGO_GOOGLE_CLIENT_SECRET"),
            redirect_uri: optional_env("MAILGO_GOOGLE_REDIRECT_URI")
                .unwrap_or_else(|| DEFAULT_REDIRECT_URI.into()),
            authorization_endpoint: "https://accounts.google.com/o/oauth2/v2/auth",
            token_endpoint: "https://oauth2.googleapis.com/token",
            scopes: "openid email https://mail.google.com/",
        }),
        ProviderKind::Outlook => Ok(ProviderOAuthConfig {
            client_id: required_env("MAILGO_OUTLOOK_CLIENT_ID")?,
            client_secret: optional_env("MAILGO_OUTLOOK_CLIENT_SECRET"),
            redirect_uri: optional_env("MAILGO_OUTLOOK_REDIRECT_URI")
                .unwrap_or_else(|| DEFAULT_REDIRECT_URI.into()),
            authorization_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            scopes: "openid email offline_access https://outlook.office365.com/IMAP.AccessAsUser.All https://outlook.office365.com/SMTP.Send",
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

fn spawn_loopback_listener(redirect_uri: &str, callback: Arc<Mutex<Option<CallbackResult>>>) {
    let Ok(parsed) = url::Url::parse(redirect_uri) else {
        return;
    };
    if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") {
        return;
    }
    let Some(port) = parsed.port_or_known_default() else {
        return;
    };
    let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
        tracing::debug!(
            port,
            "OAuth loopback port unavailable; manual code entry remains enabled"
        );
        return;
    };
    let expected_path = parsed.path().to_string();
    let _ = thread::Builder::new()
        .name("mailgo-oauth-callback".into())
        .spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Some(request_target) = read_request_target(&mut stream) else {
                return;
            };
            let callback_result = parse_callback(&request_target, &expected_path);
            let body = match &callback_result {
                CallbackResult::Code { .. } => {
                    "<h1>MailGo 授权完成</h1><p>可以返回 MailGo 继续同步。</p>"
                }
                CallbackResult::Error(_) => {
                    "<h1>MailGo 授权失败</h1><p>可以返回 MailGo 重试。</p>"
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            let _ = stream.write_all(response.as_bytes());
            if let Ok(mut stored) = callback.lock() {
                *stored = Some(callback_result);
            }
        });
}

fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = [0u8; 16 * 1024];
    let size = stream.read(&mut buffer).ok()?;
    let request = std::str::from_utf8(&buffer[..size]).ok()?;
    request
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
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
}
