use anyhow::{anyhow, Context, Result};
use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Message, SmtpTransport, Transport};

use crate::providers::{Authentication, ProviderProfile, TransportSecurity};

const MAX_RECIPIENTS_PER_FIELD: usize = 50;
const MAX_RECIPIENTS_PER_MESSAGE: usize = 100;

/// Sending can fail after the message has been fully validated, for example when a laptop goes
/// offline or an SMTP service temporarily throttles the connection. These failures are safe to
/// hand to the encrypted outbox; authentication and message-construction failures are not.
pub fn is_retryable_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "connection aborted",
        "broken pipe",
        "temporarily unavailable",
        "network is unreachable",
        "could not resolve",
        "dns",
        "rate limit",
        "too many requests",
        "throttl",
        "try again later",
        "smtp response code: 421",
        "smtp response code: 450",
        "smtp response code: 451",
        "smtp response code: 452",
        "smtp response code: 503",
        "smtp response code: 554",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// Extract a numeric Retry-After hint when the transport includes one in its error text. The
/// value is capped by the outbox caller and is never persisted as provider response text.
pub fn retry_after_seconds(error: &anyhow::Error) -> Option<u64> {
    let message = error.to_string().to_ascii_lowercase();
    for marker in ["retry-after", "retry after", "retry_after"] {
        let Some(start) = message.find(marker) else {
            continue;
        };
        let digits = message[start + marker.len()..]
            .chars()
            .skip_while(|character| !character.is_ascii_digit())
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        if let Ok(seconds) = digits.parse::<u64>() {
            return Some(seconds);
        }
    }
    None
}

pub struct OutgoingAttachment {
    pub file_name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

pub struct OutgoingMessage<'a> {
    pub from: &'a str,
    pub credential: &'a str,
    pub to: &'a str,
    pub cc: Option<&'a str>,
    pub bcc: Option<&'a str>,
    pub subject: &'a str,
    pub text_body: &'a str,
    pub html_body: Option<&'a str>,
}

/// Send one user-confirmed message through the selected provider. The credential is borrowed for
/// this call only and is never serialized or logged.
pub fn send_message(
    profile: ProviderProfile,
    message: &OutgoingMessage<'_>,
    attachments: &[OutgoingAttachment],
) -> Result<()> {
    if message.credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before sending"));
    }
    let from_mailbox: Mailbox = message.from.parse().context("invalid sender address")?;
    let to_mailboxes = parse_recipients(message.to, "recipient")?;
    let cc_mailboxes = parse_optional_recipients(message.cc, "CC recipient")?;
    let bcc_mailboxes = parse_optional_recipients(message.bcc, "BCC recipient")?;
    if to_mailboxes.len() + cc_mailboxes.len() + bcc_mailboxes.len() > MAX_RECIPIENTS_PER_MESSAGE {
        return Err(anyhow!(
            "a message can contain at most {MAX_RECIPIENTS_PER_MESSAGE} recipients"
        ));
    }
    let builder = to_mailboxes
        .into_iter()
        .fold(Message::builder().from(from_mailbox), |builder, mailbox| {
            builder.to(mailbox)
        });
    let builder = cc_mailboxes
        .into_iter()
        .fold(builder, |builder, mailbox| builder.cc(mailbox));
    let builder = bcc_mailboxes
        .into_iter()
        .fold(builder, |builder, mailbox| builder.bcc(mailbox))
        .subject(message.subject);
    let built_message = if attachments.is_empty() {
        match message.html_body.filter(|body| !body.trim().is_empty()) {
            Some(html) => builder.multipart(MultiPart::alternative_plain_html(
                message.text_body.to_string(),
                html.to_string(),
            ))?,
            None => builder.body(message.text_body.to_string())?,
        }
    } else {
        let body = match message.html_body.filter(|body| !body.trim().is_empty()) {
            Some(html) => {
                MultiPart::alternative_plain_html(message.text_body.to_string(), html.to_string())
            }
            None => MultiPart::mixed().singlepart(lettre::message::SinglePart::plain(
                message.text_body.to_string(),
            )),
        };
        let multipart = attachments.iter().try_fold(
            MultiPart::mixed().multipart(body),
            |multipart, attachment| {
                let content_type =
                    ContentType::parse(&attachment.content_type).unwrap_or_else(|_| {
                        ContentType::parse("application/octet-stream")
                            .expect("valid fallback MIME type")
                    });
                Ok::<_, anyhow::Error>(
                    multipart.singlepart(
                        Attachment::new(attachment.file_name.clone())
                            .body(attachment.bytes.clone(), content_type),
                    ),
                )
            },
        )?;
        builder.multipart(multipart)?
    };

    let credentials = Credentials::new(
        message.from.to_string(),
        crate::oauth::access_token(message.credential),
    );
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
    transport
        .build()
        .send(&built_message)
        .context("send message")?;
    Ok(())
}

fn parse_optional_recipients(value: Option<&str>, field: &str) -> Result<Vec<Mailbox>> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| parse_recipients(value, field))
        .transpose()
        .map(|value| value.unwrap_or_default())
}

fn parse_recipients(value: &str, field: &str) -> Result<Vec<Mailbox>> {
    let parts = value
        .split([',', ';', '\n', '\r'])
        .map(str::trim)
        .collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        return Err(anyhow!("{field} list contains an empty address"));
    }
    if parts.len() > MAX_RECIPIENTS_PER_FIELD {
        return Err(anyhow!(
            "a {field} list can contain at most {MAX_RECIPIENTS_PER_FIELD} addresses"
        ));
    }
    parts
        .into_iter()
        .map(|part| {
            part.parse::<Mailbox>()
                .with_context(|| format!("invalid {field} address"))
        })
        .collect()
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

        let attachment = Message::builder()
            .from("person@example.com".parse::<Mailbox>().unwrap())
            .to("recipient@example.com".parse::<Mailbox>().unwrap())
            .subject("Attachment")
            .multipart(
                MultiPart::mixed()
                    .singlepart(lettre::message::SinglePart::plain("text".to_string()))
                    .singlepart(
                        Attachment::new("hello.txt".to_string())
                            .body(b"hello".to_vec(), ContentType::parse("text/plain").unwrap()),
                    ),
            )
            .unwrap();
        assert!(attachment
            .formatted()
            .windows(5)
            .any(|window| window == b"hello"));
    }

    #[test]
    fn parses_multiple_recipient_fields_without_transport() {
        let to = parse_recipients("one@example.com; Two <two@example.com>", "recipient")
            .expect("recipient list");
        let cc =
            parse_optional_recipients(Some("copy@example.com"), "CC recipient").expect("cc list");
        let bcc = parse_optional_recipients(Some("blind@example.com"), "BCC recipient")
            .expect("bcc list");
        assert_eq!(to.len(), 2);
        assert_eq!(cc.len(), 1);
        assert_eq!(bcc.len(), 1);
        assert!(parse_recipients("one@example.com,", "recipient").is_err());
    }
}
