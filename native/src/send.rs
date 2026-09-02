use anyhow::{anyhow, Context, Result};
use lettre::message::{header::ContentType, Attachment, Mailbox, MultiPart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Message, SmtpTransport, Transport};
use std::collections::HashSet;
use std::time::Duration;

use crate::providers::{Authentication, ProviderProfile, TransportSecurity};

const MAX_RECIPIENTS_PER_FIELD: usize = 50;
const MAX_RECIPIENTS_PER_MESSAGE: usize = 100;
const SMTP_IO_TIMEOUT: Duration = Duration::from_secs(60);
const SMTP_DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(20);

/// Sending can fail after the message has been fully validated, for example when a laptop goes
/// offline or an SMTP service temporarily throttles the connection. These failures are safe to
/// hand to the encrypted outbox; authentication and message-construction failures are not.
pub fn is_retryable_error(error: &anyhow::Error) -> bool {
    if let Some(smtp_error) = error
        .chain()
        .find_map(|source| source.downcast_ref::<lettre::transport::smtp::Error>())
    {
        if smtp_error.is_permanent() {
            return false;
        }
        if smtp_error.is_transient() || smtp_error.is_timeout() {
            return true;
        }
    }

    let message = error_chain_text(error);
    if [
        "authentication",
        "authorization",
        "invalid credential",
        "requires authorization",
        "auth failed",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        return false;
    }
    if let Some(code) = smtp_response_code(&message) {
        if code == 552 && message.contains("too many recipients") {
            return true;
        }
        return (400..500).contains(&code);
    }
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
        "user-rate limit exceeded",
        "bandwidth limit exceeded",
        "too many requests",
        "too many connections",
        "maximum number of connections",
        "connection limit exceeded",
        "throttl",
        "server busy",
        "system busy",
        "try again later",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// Extract an SMTP reply code only from transport-shaped error text. RFC 5321 requires clients
/// to decide retryability from the first digit, not from provider-specific prose.
fn smtp_response_code(message: &str) -> Option<u16> {
    for marker in [
        "smtp response code:",
        "transient error (",
        "permanent error (",
    ] {
        let Some(start) = message.find(marker) else {
            continue;
        };
        let digits = message[start + marker.len()..]
            .chars()
            .skip_while(|character| !character.is_ascii_digit())
            .take(3)
            .collect::<String>();
        if digits.len() != 3 {
            continue;
        }
        if let Ok(code @ 200..=599) = digits.parse::<u16>() {
            return Some(code);
        }
    }
    None
}

fn error_chain_text(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|source| source.to_string())
        .collect::<Vec<_>>()
        .join(": ")
        .to_ascii_lowercase()
}

/// Return a stable, privacy-safe SMTP diagnostic category. Provider response text can contain
/// account-specific information, so only this bounded category is sent to the renderer or log.
pub fn error_category(error: &anyhow::Error) -> &'static str {
    let message = error_chain_text(error);
    let response_code = smtp_response_code(&message);
    if matches!(response_code, Some(530 | 534 | 535 | 538))
        || [
            "authentication",
            "authorization",
            "invalid credential",
            "requires authorization",
            "auth failed",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    {
        return "authentication";
    }
    if [
        "rate limit",
        "too many requests",
        "too many connections",
        "connection limit exceeded",
        "throttl",
        "server busy",
        "system busy",
        "try again later",
    ]
    .iter()
    .any(|marker| message.contains(marker))
    {
        return "rate-limit";
    }
    if is_retryable_error(error) {
        return "network";
    }
    if error.chain().any(|source| {
        source
            .downcast_ref::<lettre::transport::smtp::Error>()
            .is_some_and(|smtp| smtp.is_tls())
    }) {
        return "tls";
    }
    "provider"
}

/// Extract a numeric Retry-After hint when the transport includes one in its error text. The
/// value is capped by the outbox caller and is never persisted as provider response text.
pub fn retry_after_seconds(error: &anyhow::Error) -> Option<u64> {
    let message = error_chain_text(error);
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
    pub content_id: Option<String>,
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
    pub in_reply_to: Option<&'a str>,
    pub references: &'a [String],
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
    let built_message = build_message(message, attachments)?;

    smtp_transport(&profile, message.from, message.credential, SMTP_IO_TIMEOUT)?
        .send(&built_message)
        .context("send message")?;
    Ok(())
}

fn smtp_transport(
    profile: &ProviderProfile,
    email: &str,
    credential: &str,
    timeout: Duration,
) -> Result<SmtpTransport> {
    if credential.trim().is_empty() {
        return Err(anyhow!("account requires authorization before connecting"));
    }
    let credentials = Credentials::new(email.to_string(), crate::oauth::access_token(credential));
    let mut transport = match profile.smtp.security {
        TransportSecurity::Tls => SmtpTransport::relay(&profile.smtp.host),
        TransportSecurity::StartTls => SmtpTransport::starttls_relay(&profile.smtp.host),
    }
    .with_context(|| format!("configure SMTP host {}", profile.smtp.host))?
    .port(profile.smtp.port)
    .timeout(Some(timeout))
    .credentials(credentials);

    transport = match profile.authentication {
        Authentication::OAuth2 => transport.authentication(vec![Mechanism::Xoauth2]),
        Authentication::AppPassword | Authentication::Password => {
            transport.authentication(vec![Mechanism::Plain, Mechanism::Login])
        }
    };
    Ok(transport.build())
}

/// Authenticate to the configured SMTP server and issue NOOP. This validates outgoing-mail
/// credentials without constructing or transmitting a message envelope.
pub fn test_connection(profile: &ProviderProfile, email: &str, credential: &str) -> Result<()> {
    let connected = smtp_transport(profile, email, credential, SMTP_DIAGNOSTIC_TIMEOUT)?
        .test_connection()
        .context("test SMTP connection")?;
    if !connected {
        return Err(anyhow!("SMTP server rejected the connection test"));
    }
    Ok(())
}

fn build_message(
    message: &OutgoingMessage<'_>,
    attachments: &[OutgoingAttachment],
) -> Result<Message> {
    validate_thread_headers(message.in_reply_to, message.references)?;
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
    let builder = if let Some(message_id) = message.in_reply_to {
        builder.in_reply_to(format!("<{message_id}>"))
    } else {
        builder
    };
    let builder = if message.references.is_empty() {
        builder
    } else {
        builder.references(
            message
                .references
                .iter()
                .map(|message_id| format!("<{message_id}>"))
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    let html_body = message.html_body.filter(|body| !body.trim().is_empty());
    for attachment in attachments {
        if let Some(content_id) = attachment.content_id.as_deref() {
            validate_content_id(content_id)?;
        }
    }
    let inline_attachments = attachments
        .iter()
        .filter(|attachment| html_body.is_some() && attachment.content_id.is_some())
        .collect::<Vec<_>>();
    let regular_attachments = attachments
        .iter()
        .filter(|attachment| html_body.is_none() || attachment.content_id.is_none())
        .collect::<Vec<_>>();

    let built_message = if attachments.is_empty() {
        match html_body {
            Some(html) => builder.multipart(MultiPart::alternative_plain_html(
                message.text_body.to_string(),
                html.to_string(),
            ))?,
            None => builder.body(message.text_body.to_string())?,
        }
    } else {
        let body = match html_body {
            Some(html) if !inline_attachments.is_empty() => inline_attachments.iter().try_fold(
                MultiPart::related().multipart(MultiPart::alternative_plain_html(
                    message.text_body.to_string(),
                    html.to_string(),
                )),
                |multipart, attachment| {
                    let content_id = attachment
                        .content_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("inline attachment is missing a content id"))?;
                    let content_type = content_type_or_octet_stream(&attachment.content_type);
                    Ok::<_, anyhow::Error>(
                        multipart.singlepart(
                            Attachment::new_inline_with_name(
                                content_id.to_string(),
                                attachment.file_name.clone(),
                            )
                            .body(attachment.bytes.clone(), content_type),
                        ),
                    )
                },
            )?,
            Some(html) => {
                MultiPart::alternative_plain_html(message.text_body.to_string(), html.to_string())
            }
            None => MultiPart::mixed().singlepart(lettre::message::SinglePart::plain(
                message.text_body.to_string(),
            )),
        };
        if regular_attachments.is_empty() {
            builder.multipart(body)?
        } else {
            let multipart = regular_attachments.iter().try_fold(
                MultiPart::mixed().multipart(body),
                |multipart, attachment| {
                    let content_type = content_type_or_octet_stream(&attachment.content_type);
                    Ok::<_, anyhow::Error>(
                        multipart.singlepart(
                            Attachment::new(attachment.file_name.clone())
                                .body(attachment.bytes.clone(), content_type),
                        ),
                    )
                },
            )?;
            builder.multipart(multipart)?
        }
    };

    Ok(built_message)
}

pub fn validate_thread_headers(in_reply_to: Option<&str>, references: &[String]) -> Result<()> {
    if references.len() > crate::mail::MAX_THREAD_REFERENCES {
        return Err(anyhow!("reply contains too many message references"));
    }
    if let Some(message_id) = in_reply_to {
        if crate::mail::safe_message_id(message_id).as_deref() != Some(message_id) {
            return Err(anyhow!("reply message id is unsafe"));
        }
    }
    let mut seen = HashSet::with_capacity(references.len());
    for message_id in references {
        if crate::mail::safe_message_id(message_id).as_deref() != Some(message_id.as_str())
            || !seen.insert(message_id.as_str())
        {
            return Err(anyhow!("reply reference is unsafe or duplicated"));
        }
    }
    Ok(())
}

fn content_type_or_octet_stream(value: &str) -> ContentType {
    ContentType::parse(value).unwrap_or_else(|_| {
        ContentType::parse("application/octet-stream").expect("valid fallback MIME type")
    })
}

fn validate_content_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '@')
        })
    {
        return Err(anyhow!("inline attachment content id is unsafe"));
    }
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
    fn builds_related_mime_for_inline_images_and_keeps_regular_attachments() {
        let message = OutgoingMessage {
            from: "person@example.com",
            credential: "unused-in-unit-test",
            to: "recipient@example.com",
            cc: None,
            bcc: None,
            subject: "Inline image",
            text_body: "body",
            html_body: Some("<p><img src=\"cid:mailgo-inline-1\"></p>"),
            in_reply_to: None,
            references: &[],
        };
        let attachments = vec![
            OutgoingAttachment {
                file_name: "pixel.png".into(),
                content_type: "image/png".into(),
                content_id: Some("mailgo-inline-1".into()),
                bytes: vec![0, 1, 2],
            },
            OutgoingAttachment {
                file_name: "notes.txt".into(),
                content_type: "text/plain".into(),
                content_id: None,
                bytes: b"notes".to_vec(),
            },
        ];
        let formatted =
            String::from_utf8_lossy(&build_message(&message, &attachments).unwrap().formatted())
                .into_owned();
        assert!(formatted.contains("multipart/mixed"));
        assert!(formatted.contains("multipart/related"));
        assert!(formatted.contains("cid:mailgo-inline-1"));
        assert!(formatted.contains("Content-ID: <mailgo-inline-1>"));
        assert!(formatted.contains("filename=\"notes.txt\""));
    }

    #[test]
    fn rejects_unsafe_inline_content_ids() {
        let message = OutgoingMessage {
            from: "person@example.com",
            credential: "unused-in-unit-test",
            to: "recipient@example.com",
            cc: None,
            bcc: None,
            subject: "Inline image",
            text_body: "body",
            html_body: Some("<p>body</p>"),
            in_reply_to: None,
            references: &[],
        };
        let attachment = OutgoingAttachment {
            file_name: "pixel.png".into(),
            content_type: "image/png".into(),
            content_id: Some("bad\r\nid".into()),
            bytes: vec![0, 1, 2],
        };
        assert!(build_message(&message, &[attachment]).is_err());
    }

    #[test]
    fn builds_bounded_rfc_reply_headers() {
        let references = vec!["root@example.com".into(), "parent@example.com".into()];
        let message = OutgoingMessage {
            from: "person@example.com",
            credential: "unused-in-unit-test",
            to: "recipient@example.com",
            cc: None,
            bcc: None,
            subject: "Re: Project",
            text_body: "body",
            html_body: None,
            in_reply_to: Some("parent@example.com"),
            references: &references,
        };
        let formatted = String::from_utf8_lossy(&build_message(&message, &[]).unwrap().formatted())
            .into_owned();
        assert!(formatted.contains("In-Reply-To: <parent@example.com>"));
        assert!(formatted.contains("References: <root@example.com> <parent@example.com>"));
    }

    #[test]
    fn rejects_injected_or_unbounded_reply_headers() {
        assert!(validate_thread_headers(
            Some("parent@example.com\r\nBcc: hidden@example.com"),
            &[]
        )
        .is_err());
        let too_many = (0..=crate::mail::MAX_THREAD_REFERENCES)
            .map(|index| format!("message-{index}@example.com"))
            .collect::<Vec<_>>();
        assert!(validate_thread_headers(None, &too_many).is_err());
        assert!(validate_thread_headers(
            None,
            &[
                "duplicate@example.com".into(),
                "duplicate@example.com".into()
            ]
        )
        .is_err());
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

    #[test]
    fn retries_transient_smtp_codes_but_not_permanent_failures() {
        for code in [421, 450, 451, 452] {
            assert!(is_retryable_error(&anyhow!(
                "send message: SMTP response code: {code}"
            )));
        }
        for code in [503, 535, 550, 554] {
            assert!(!is_retryable_error(&anyhow!(
                "send message: SMTP response code: {code}"
            )));
        }
        assert!(is_retryable_error(&anyhow!(
            "send message: SMTP response code: 552 too many recipients"
        )));
        assert!(!is_retryable_error(&anyhow!(
            "send message: SMTP response code: 552 message too large"
        )));
    }

    #[test]
    fn recognizes_lettre_and_provider_throttling_error_shapes() {
        assert!(is_retryable_error(&anyhow!(
            "send message: transient error (451): server busy"
        )));
        assert!(!is_retryable_error(&anyhow!(
            "send message: permanent error (554): transaction failed"
        )));
        assert!(!is_retryable_error(&anyhow!(
            "SMTP authentication failed: user-rate limit exceeded"
        )));
        assert!(is_retryable_error(&anyhow!(
            "maximum number of connections reached"
        )));
        assert!(is_retryable_error(&anyhow!(
            "mailbox is throttled; retry later"
        )));
        assert!(is_retryable_error(
            &anyhow!("SMTP response code: 451 server busy").context("send message")
        ));
        assert_eq!(
            retry_after_seconds(&anyhow!("Retry-After: 45").context("send message")),
            Some(45)
        );
    }

    #[test]
    fn diagnostic_errors_are_reduced_to_stable_privacy_safe_categories() {
        assert_eq!(
            error_category(&anyhow!(
                "permanent error (535): Authentication failed for diagnostic account"
            )),
            "authentication"
        );
        assert_eq!(
            error_category(&anyhow!("server busy; try again later")),
            "rate-limit"
        );
        assert_eq!(error_category(&anyhow!("connection refused")), "network");
        assert_eq!(
            error_category(&anyhow!("permanent error (550): mailbox unavailable")),
            "provider"
        );
    }
}
