use std::borrow::Cow;
use std::collections::HashSet;

use ammonia::Builder;
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use mail_parser::{Address, Message, MessageParser, MimeHeaders, PartType};
use serde::{Deserialize, Serialize};

use crate::classifier::{classify, SmartCategory};

const MAX_CACHED_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_PREVIEW_CHARS: usize = 240;
pub const MAX_FULL_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ATTACHMENTS_PER_MESSAGE: usize = 64;
const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ATTACHMENT_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_ATTACHMENT_NAME_CHARS: usize = 255;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedAttachment {
    #[serde(default)]
    pub index: usize,
    pub file_name: String,
    pub content_type: String,
    pub content_id: Option<String>,
    pub size: usize,
    #[serde(default)]
    pub cache_path: Option<String>,
}

pub struct AttachmentPayload {
    pub content_type: String,
    pub content_id: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedMessage {
    pub id: String,
    pub account_id: String,
    pub folder: String,
    pub uid: u32,
    pub subject: String,
    pub sender_name: String,
    pub sender_email: String,
    pub received_at: Option<String>,
    pub unread: bool,
    pub starred: bool,
    pub category: SmartCategory,
    pub is_ad: bool,
    pub preview: String,
    pub text_body: String,
    pub html_body: Option<String>,
    pub attachments: Vec<CachedAttachment>,
    pub raw_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedMailbox {
    pub schema_version: u32,
    pub account_id: String,
    pub folder: String,
    pub uid_validity: Option<u32>,
    pub synced_at: String,
    pub messages: Vec<CachedMessage>,
    #[serde(default)]
    pub oldest_uid: Option<u32>,
    #[serde(default)]
    pub has_more: bool,
}

impl CachedMailbox {
    pub fn empty(account_id: impl Into<String>, folder: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            account_id: account_id.into(),
            folder: folder.into(),
            uid_validity: None,
            synced_at: String::new(),
            messages: Vec::new(),
            oldest_uid: None,
            has_more: false,
        }
    }
}

/// Parse a header-only IMAP fetch. Header parsing stays provider-neutral and is safe to use for
/// list views without downloading message bodies.
pub fn parse_header(
    account_id: &str,
    folder: &str,
    uid: u32,
    unread: bool,
    starred: bool,
    header: &[u8],
) -> Result<CachedMessage> {
    let parsed = MessageParser::default()
        .parse_headers(header)
        .ok_or_else(|| anyhow!("message headers could not be parsed"))?;
    build_message(account_id, folder, uid, unread, starred, &parsed, None)
}

/// Parse a full RFC 5322 message. The raw message is never returned to the renderer; only a
/// bounded, sanitized representation is retained in the offline cache.
pub fn parse_full(
    account_id: &str,
    folder: &str,
    uid: u32,
    unread: bool,
    starred: bool,
    raw: &[u8],
) -> Result<CachedMessage> {
    if raw.len() > MAX_FULL_MESSAGE_BYTES {
        return Err(anyhow!("message exceeds the safe MIME size limit"));
    }
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("message could not be parsed"))?;
    build_message(account_id, folder, uid, unread, starred, &parsed, Some(raw))
}

fn build_message(
    account_id: &str,
    folder: &str,
    uid: u32,
    unread: bool,
    starred: bool,
    parsed: &Message<'_>,
    _raw: Option<&[u8]>,
) -> Result<CachedMessage> {
    let (sender_name, sender_email) = address_parts(parsed.from());
    let subject = parsed.subject().unwrap_or("(无主题)").trim().to_string();
    let text_body = parsed.body_text(0).map(Cow::into_owned).unwrap_or_default();
    let html_body = parsed
        .body_html(0)
        .map(|html| sanitize_html(html.as_ref()))
        .filter(|html| !html.is_empty())
        .map(|html| html.chars().take(MAX_CACHED_HTML_BYTES).collect());
    let preview = text_body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_PREVIEW_CHARS)
        .collect();
    let has_list_unsubscribe = parsed
        .headers_raw()
        .any(|(name, _)| name.eq_ignore_ascii_case("list-unsubscribe"));
    let classification = classify(
        if sender_email.is_empty() {
            &sender_name
        } else {
            &sender_email
        },
        &subject,
        has_list_unsubscribe,
    );
    let attachments = parsed
        .attachments()
        .enumerate()
        .map(|part| CachedAttachment {
            index: part.0,
            file_name: safe_attachment_name(part.1.attachment_name().unwrap_or("attachment")),
            content_type: content_type_for_part(part.1),
            content_id: part.1.content_id().map(str::to_string),
            size: part_size(part.1),
            cache_path: None,
        })
        .collect();

    let message_id = parsed
        .message_id()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{account_id}-{folder}-{uid}"));

    Ok(CachedMessage {
        id: message_id,
        account_id: account_id.to_string(),
        folder: folder.to_string(),
        uid,
        subject,
        sender_name,
        sender_email,
        received_at: parsed.date().map(|date| date.to_rfc3339()),
        unread,
        starred,
        category: classification.category,
        is_ad: classification.is_ad,
        preview,
        text_body,
        html_body,
        attachments,
        // Raw MIME is never exposed to the renderer; attachment bytes are stored separately in
        // the encrypted cache when a full message fetch populates them.
        raw_path: None,
    })
}

pub fn extract_attachment_payloads(raw: &[u8]) -> Result<Vec<AttachmentPayload>> {
    if raw.len() > MAX_FULL_MESSAGE_BYTES {
        return Err(anyhow!("message exceeds the safe MIME size limit"));
    }
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("message could not be parsed"))?;
    let mut payloads = Vec::new();
    let mut total_bytes = 0usize;
    for (index, part) in parsed.attachments().enumerate() {
        if index >= MAX_ATTACHMENTS_PER_MESSAGE {
            return Err(anyhow!("message contains too many attachments"));
        }
        let size = part_size(part);
        if size > MAX_ATTACHMENT_BYTES
            || total_bytes.saturating_add(size) > MAX_ATTACHMENT_TOTAL_BYTES
        {
            return Err(anyhow!("message attachments exceed the safe size limit"));
        }
        total_bytes = total_bytes.saturating_add(size);
        payloads.push(AttachmentPayload {
            content_type: content_type_for_part(part),
            content_id: part.content_id().map(str::to_string),
            bytes: part_bytes(part),
        });
    }
    Ok(payloads)
}

pub fn embed_inline_images(html: &mut Option<String>, payloads: &[AttachmentPayload]) {
    let Some(html) = html.as_mut() else { return };
    for payload in payloads {
        if !payload
            .content_type
            .to_ascii_lowercase()
            .starts_with("image/")
            || payload.bytes.len() > 1024 * 1024
        {
            continue;
        }
        let Some(content_id) = payload.content_id.as_deref() else {
            continue;
        };
        let encoded = STANDARD.encode(&payload.bytes);
        let data_uri = format!("data:{};base64,{}", payload.content_type, encoded);
        for needle in [format!("cid:{content_id}"), format!("cid:<{content_id}>")] {
            let replaced = html.replace(&needle, &data_uri);
            *html = replaced;
        }
    }
}

fn address_parts(address: Option<&Address<'_>>) -> (String, String) {
    let Some(first) = address.and_then(Address::first) else {
        return (String::new(), String::new());
    };
    (
        first.name().unwrap_or_default().to_string(),
        first.address().unwrap_or_default().to_string(),
    )
}

fn part_size(part: &mail_parser::MessagePart<'_>) -> usize {
    match &part.body {
        PartType::Text(value) | PartType::Html(value) => value.len(),
        PartType::Binary(value) | PartType::InlineBinary(value) => value.len(),
        PartType::Message(message) => message.raw_message().len(),
        PartType::Multipart(_) => 0,
    }
}

fn content_type_for_part(part: &mail_parser::MessagePart<'_>) -> String {
    part.content_type()
        .map(|kind| {
            kind.c_subtype
                .as_ref()
                .map(|subtype| format!("{}/{}", kind.c_type, subtype))
                .unwrap_or_else(|| kind.c_type.to_string())
        })
        .filter(|value| {
            value.len() <= 128
                && value
                    .chars()
                    .all(|character| !matches!(character, '\r' | '\n' | '\0'))
        })
        .unwrap_or_else(|| "application/octet-stream".into())
}

fn safe_attachment_name(value: &str) -> String {
    let mut name = value
        .chars()
        .map(|character| {
            if character == '/' || character == '\\' || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .take(MAX_ATTACHMENT_NAME_CHARS)
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();
    if name.is_empty() {
        name = "attachment".into();
    }
    name
}

fn part_bytes(part: &mail_parser::MessagePart<'_>) -> Vec<u8> {
    match &part.body {
        PartType::Text(value) => value.as_bytes().to_vec(),
        PartType::Html(value) => value.as_bytes().to_vec(),
        PartType::Binary(value) | PartType::InlineBinary(value) => value.to_vec(),
        PartType::Message(message) => message.raw_message().to_vec(),
        PartType::Multipart(_) => Vec::new(),
    }
}

/// The only HTML that crosses the IPC boundary is cleaned with a strict allowlist. Safe HTTPS image
/// sources are retained so the renderer can honor the user's explicit remote-image preference;
/// the renderer blocks them by default. Inline `cid:` images are resolved only from the same MIME
/// message.
pub fn sanitize_html(input: &str) -> String {
    let mut allowed_schemes = HashSet::new();
    allowed_schemes.insert("cid");
    allowed_schemes.insert("https");
    allowed_schemes.insert("mailto");
    Builder::default()
        .url_schemes(allowed_schemes)
        .attribute_filter(|element, attribute, value| {
            let element = element.to_ascii_lowercase();
            let attribute = attribute.to_ascii_lowercase();
            if (element == "img" && attribute == "srcset")
                || matches!(
                    attribute.as_str(),
                    "style" | "srcdoc" | "ping" | "formaction" | "xlink:href"
                )
            {
                return None;
            }
            if element == "img" && attribute == "src" {
                let normalized = value.to_ascii_lowercase();
                if normalized.starts_with("cid:") || normalized.starts_with("https://") {
                    return Some(Cow::Borrowed(value));
                }
                return None;
            }
            Some(Cow::Borrowed(value))
        })
        .link_rel(Some("noreferrer noopener"))
        .clean(input)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "From: no-reply@apple.com\r\nSubject: Your Apple Account was used to sign in\r\nMessage-ID: <mailgo-test@example.com>\r\nDate: Tue, 01 Jan 2026 12:00:00 +0000\r\nList-Unsubscribe: <https://example.com/unsubscribe>\r\nContent-Type: multipart/alternative; boundary=mailgo\r\n\r\n--mailgo\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nSecurity notice\r\n--mailgo\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Safe <strong>notice</strong></p><script>alert(1)</script>\r\n--mailgo--\r\n";

    #[test]
    fn parses_and_sanitizes_full_message() {
        let message = parse_full("account", "INBOX", 7, true, false, RAW.as_bytes()).unwrap();
        assert_eq!(message.id, "mailgo-test@example.com");
        assert_eq!(message.category, SmartCategory::AppleConnect);
        assert_eq!(message.sender_email, "no-reply@apple.com");
        let html = message.html_body.unwrap();
        assert!(html.contains("<strong>notice</strong>"));
        assert!(!html.contains("script"));
    }

    #[test]
    fn strips_javascript_links() {
        let html = sanitize_html("<a href=\"javascript:alert(1)\">bad</a><p>ok</p>");
        assert!(!html.contains("javascript:"));
        assert!(html.contains("ok"));
    }

    #[test]
    fn retains_safe_remote_images_but_blocks_unsafe_attributes_and_links() {
        let html = sanitize_html(
            r#"<p style="background:url(https://tracker.example/pixel)">ok</p><img src="https://tracker.example/pixel" srcset="https://tracker.example/2x"><a href="mailto:person@example.com" target="_blank">contact</a><img src="cid:logo@example.com">"#,
        );
        assert!(html.contains("<img src=\"https://tracker.example/pixel\""));
        assert!(!html.contains("srcset="));
        assert!(!html.contains("style="));
        assert!(html.contains("mailto:person@example.com"));
        assert!(html.contains("cid:logo@example.com"));
        assert!(html.contains("noreferrer noopener"));
    }

    #[test]
    fn removes_non_https_image_sources() {
        let html = sanitize_html(
            r#"<img src="http://tracker.example/pixel"><img src="data:text/html,evil"><img src="javascript:alert(1)"><p>ok</p>"#,
        );
        assert!(!html.contains("tracker.example"));
        assert!(!html.contains("data:text/html"));
        assert!(!html.contains("javascript:"));
        assert!(html.contains("ok"));
    }

    #[test]
    fn embeds_small_inline_cid_images_without_exposing_raw_mime() {
        let raw = "From: sender@example.com\r\nSubject: Inline image\r\nContent-Type: multipart/related; boundary=related\r\n\r\n--related\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p><img src=\"cid:logo@example.com\" alt=\"logo\"></p>\r\n--related\r\nContent-Type: image/png\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: inline; filename=\"logo.png\"\r\nContent-ID: <logo@example.com>\r\n\r\niVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\r\n--related--\r\n";
        let mut message = parse_full("account", "INBOX", 8, false, false, raw.as_bytes()).unwrap();
        let payloads = extract_attachment_payloads(raw.as_bytes()).unwrap();
        assert_eq!(payloads.len(), 1);
        embed_inline_images(&mut message.html_body, &payloads);
        let html = message.html_body.unwrap();
        assert!(html.contains("data:image/png;base64,"));
        assert_eq!(message.attachments.len(), 1);
    }

    #[test]
    fn normalizes_untrusted_attachment_names() {
        let raw = "From: sender@example.com\r\nSubject: Attachment\r\nContent-Type: multipart/mixed; boundary=mixed\r\n\r\n--mixed\r\nContent-Type: text/plain; charset=utf-8\r\n\r\ntext\r\n--mixed\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"../../private.txt\"\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--mixed--\r\n";
        let message = parse_full("account", "INBOX", 9, false, false, raw.as_bytes()).unwrap();
        assert_eq!(message.attachments.len(), 1);
        let name = &message.attachments[0].file_name;
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert!(!name.starts_with('.'));
    }

    #[test]
    fn legacy_mailbox_cache_defaults_cursor_fields() {
        let mailbox: CachedMailbox = serde_json::from_str(
            r#"{
                "schemaVersion": 1,
                "accountId": "account",
                "folder": "INBOX",
                "uidValidity": 7,
                "syncedAt": "unix:1",
                "messages": []
            }"#,
        )
        .unwrap();
        assert_eq!(mailbox.oldest_uid, None);
        assert!(!mailbox.has_more);
    }
}
