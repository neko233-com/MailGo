use anyhow::{anyhow, Context, Result};
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Message, SmtpTransport, Transport};

use crate::providers::{Authentication, ProviderProfile, TransportSecurity};

/// Send one user-confirmed message through the selected provider. The credential is borrowed for
/// this call only and is never serialized or logged.
pub fn send_message(
    profile: ProviderProfile,
    from: &str,
    credential: &str,
    to: &str,
    subject: &str,
    text_body: &str,
    html_body: Option<&str>,
) -> Result<()> {
    if credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before sending"));
    }
    let from_mailbox: Mailbox = from.parse().context("invalid sender address")?;
    let to_mailbox: Mailbox = to.parse().context("invalid recipient address")?;
    let builder = Message::builder()
        .from(from_mailbox)
        .to(to_mailbox)
        .subject(subject);
    let message = match html_body.filter(|body| !body.trim().is_empty()) {
        Some(html) => builder.multipart(MultiPart::alternative_plain_html(
            text_body.to_string(),
            html.to_string(),
        ))?,
        None => builder.body(text_body.to_string())?,
    };

    let credentials = Credentials::new(from.to_string(), credential.to_string());
    let mut transport = match profile.smtp.security {
        TransportSecurity::Tls => SmtpTransport::relay(&profile.smtp.host),
        TransportSecurity::StartTls => SmtpTransport::starttls_relay(&profile.smtp.host),
    }
    .with_context(|| format!("configure SMTP host {}", profile.smtp.host))?
    .port(profile.smtp.port)
    .credentials(credentials);

    transport = match profile.authentication {
        Authentication::OAuth2 => transport.authentication(vec![Mechanism::Xoauth2]),
        Authentication::AppPassword | Authentication::Password => {
            transport.authentication(vec![Mechanism::Plain, Mechanism::Login])
        }
    };
    transport.build().send(&message).context("send message")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_plain_and_html_messages_without_transport() {
        let plain = Message::builder()
            .from("person@example.com".parse::<Mailbox>().unwrap())
            .to("recipient@example.com".parse::<Mailbox>().unwrap())
            .subject("Test")
            .body("text".to_string())
            .unwrap();
        assert!(plain.formatted().windows(4).any(|window| window == b"text"));

        let html = Message::builder()
            .from("person@example.com".parse::<Mailbox>().unwrap())
            .to("recipient@example.com".parse::<Mailbox>().unwrap())
            .subject("Test")
            .multipart(MultiPart::alternative_plain_html(
                "text".to_string(),
                "<p>text</p>".to_string(),
            ))
            .unwrap();
        assert!(html.formatted().windows(4).any(|window| window == b"text"));
    }
}
