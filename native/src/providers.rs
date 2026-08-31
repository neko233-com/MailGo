use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Supported first-party provider identifiers. The string representation is also the
/// stable value persisted in account configuration files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Google,
    Qq,
    Outlook,
    Other,
}

impl ProviderKind {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "google" | "gmail" => Ok(Self::Google),
            "qq" | "qqmail" => Ok(Self::Qq),
            "outlook" | "microsoft" | "office365" => Ok(Self::Outlook),
            "other" | "custom" => Ok(Self::Other),
            _ => Err(anyhow!("unsupported mail provider")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Qq => "qq",
            Self::Outlook => "outlook",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportSecurity {
    Tls,
    StartTls,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub security: TransportSecurity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Authentication {
    OAuth2,
    AppPassword,
    Password,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub provider: ProviderKind,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    pub authentication: Authentication,
    pub supports_oauth: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomConnectionSettings {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: TransportSecurity,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: TransportSecurity,
    pub authentication: Authentication,
}

impl TransportSecurity {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "tls" | "ssl" => Ok(Self::Tls),
            "starttls" | "start-tls" => Ok(Self::StartTls),
            _ => Err(anyhow!("unsupported transport security")),
        }
    }
}

impl Authentication {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "oauth2" | "oauth" | "xoauth2" => Ok(Self::OAuth2),
            "app-password" | "apppassword" | "app_password" => Ok(Self::AppPassword),
            "password" | "plain" | "login" => Ok(Self::Password),
            _ => Err(anyhow!("unsupported authentication method")),
        }
    }
}

/// Return safe, provider-neutral connection defaults. Secrets and tokens are deliberately not
/// part of this structure; they are loaded only at the moment a connection is opened.
pub fn profile_for(provider: ProviderKind) -> Result<ProviderProfile> {
    match provider {
        ProviderKind::Google => Ok(ProviderProfile {
            provider,
            imap: Endpoint {
                host: "imap.gmail.com".into(),
                port: 993,
                security: TransportSecurity::Tls,
            },
            smtp: Endpoint {
                host: "smtp.gmail.com".into(),
                port: 465,
                security: TransportSecurity::Tls,
            },
            // The quick-start UI opens Google's App Password page. An app password is a
            // provider-issued credential and is intentionally handled as IMAP/SMTP password
            // auth; a future OAuth flow can select OAuth2 without changing the transport layer.
            authentication: Authentication::AppPassword,
            supports_oauth: true,
        }),
        ProviderKind::Qq => Ok(ProviderProfile {
            provider,
            imap: Endpoint {
                host: "imap.qq.com".into(),
                port: 993,
                security: TransportSecurity::Tls,
            },
            smtp: Endpoint {
                host: "smtp.qq.com".into(),
                port: 465,
                security: TransportSecurity::Tls,
            },
            authentication: Authentication::AppPassword,
            supports_oauth: false,
        }),
        ProviderKind::Outlook => Ok(ProviderProfile {
            provider,
            imap: Endpoint {
                host: "outlook.office365.com".into(),
                port: 993,
                security: TransportSecurity::Tls,
            },
            smtp: Endpoint {
                host: "smtp.office365.com".into(),
                port: 587,
                security: TransportSecurity::StartTls,
            },
            authentication: Authentication::OAuth2,
            supports_oauth: true,
        }),
        ProviderKind::Other => Err(anyhow!(
            "custom IMAP/SMTP accounts require server settings before connecting"
        )),
    }
}

pub fn profile_for_custom(settings: &CustomConnectionSettings) -> Result<ProviderProfile> {
    validate_host(&settings.imap_host, "IMAP")?;
    validate_host(&settings.smtp_host, "SMTP")?;
    if settings.imap_host.trim().is_empty() || settings.smtp_host.trim().is_empty() {
        return Err(anyhow!("custom IMAP/SMTP hosts are required"));
    }
    if settings.imap_port == 0 || settings.smtp_port == 0 {
        return Err(anyhow!("custom IMAP/SMTP ports must be greater than zero"));
    }
    Ok(ProviderProfile {
        provider: ProviderKind::Other,
        imap: Endpoint {
            host: settings.imap_host.trim().to_string(),
            port: settings.imap_port,
            security: settings.imap_security,
        },
        smtp: Endpoint {
            host: settings.smtp_host.trim().to_string(),
            port: settings.smtp_port,
            security: settings.smtp_security,
        },
        authentication: settings.authentication,
        supports_oauth: settings.authentication == Authentication::OAuth2,
    })
}

fn validate_host(host: &str, protocol: &str) -> Result<()> {
    let host = host.trim();
    if host.is_empty() {
        return Err(anyhow!("custom {protocol} host is required"));
    }
    if host.len() > 255
        || host.chars().any(|character| {
            character.is_control() || character.is_whitespace() || character == '/'
        })
    {
        return Err(anyhow!("custom {protocol} host is invalid"));
    }
    Ok(())
}

pub fn validate_email(email: &str) -> Result<()> {
    let trimmed = email.trim();
    if trimmed.len() > 320
        || trimmed
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(anyhow!("invalid email address"));
    }
    let (local, domain) = trimmed
        .split_once('@')
        .ok_or_else(|| anyhow!("invalid email address"))?;
    if local.is_empty()
        || local.len() > 64
        || domain.is_empty()
        || domain.len() > 255
        || domain.split('@').count() != 1
        || !domain.contains('.')
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '/'
                        | '='
                        | '?'
                        | '^'
                        | '_'
                        | '`'
                        | '{'
                        | '|'
                        | '}'
                        | '~'
                        | '.'
                )
        })
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
    {
        return Err(anyhow!("invalid email address"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_defaults_cover_first_party_services() {
        let qq = profile_for(ProviderKind::Qq).expect("QQ profile");
        assert_eq!(qq.imap.host, "imap.qq.com");
        assert_eq!(qq.smtp.port, 465);
        assert_eq!(qq.authentication, Authentication::AppPassword);

        let google = profile_for(ProviderKind::Google).expect("Google profile");
        assert!(google.supports_oauth);
        assert_eq!(google.authentication, Authentication::AppPassword);
        assert_eq!(google.imap.port, 993);

        let outlook = profile_for(ProviderKind::Outlook).expect("Outlook profile");
        assert_eq!(outlook.smtp.security, TransportSecurity::StartTls);
    }

    #[test]
    fn email_validation_rejects_unsafe_shapes() {
        assert!(validate_email("person@example.com").is_ok());
        assert!(validate_email("person+tag@example.com").is_ok());
        assert!(validate_email("missing-at").is_err());
        assert!(validate_email("@example.com").is_err());
        assert!(validate_email("person@@example.com").is_err());
        assert!(validate_email("person..name@example.com").is_err());
        assert!(validate_email("person name@example.com").is_err());
        assert!(validate_email("person@example..com").is_err());
        assert!(validate_email("person@example.com\r\nBcc: attacker@example.com").is_err());
    }

    #[test]
    fn custom_profile_preserves_explicit_tls_and_ports() {
        let profile = profile_for_custom(&CustomConnectionSettings {
            imap_host: "imap.example.com".into(),
            imap_port: 143,
            imap_security: TransportSecurity::StartTls,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            smtp_security: TransportSecurity::StartTls,
            authentication: Authentication::Password,
        })
        .expect("custom profile");
        assert_eq!(profile.imap.port, 143);
        assert_eq!(profile.imap.security, TransportSecurity::StartTls);
        assert_eq!(profile.smtp.port, 587);
    }

    #[test]
    fn custom_profile_rejects_unsafe_hosts() {
        let settings = CustomConnectionSettings {
            imap_host: "imap.example.com\r\nX-Injected: value".into(),
            imap_port: 993,
            imap_security: TransportSecurity::Tls,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            smtp_security: TransportSecurity::Tls,
            authentication: Authentication::Password,
        };
        assert!(profile_for_custom(&settings).is_err());
    }
}
