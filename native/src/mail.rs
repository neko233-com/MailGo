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
            file_name: part.1.attachment_name().unwrap_or("attachment").to_string(),
            content_type: part
                .1
                .content_type()
                .map(|kind| {
                    kind.c_subtype
                        .as_ref()
                        .map(|subtype| format!("{}/{}", kind.c_type, subtype))
                        .unwrap_or_else(|| kind.c_type.to_string())
                })
                .unwrap_or_else(|| "application/octet-stream".into()),
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
        // Raw MIME and attachment bytes are intentionally not claimed as cached until the
        // streaming attachment store is enabled. The sanitized body remains offline-readable.
        raw_path: None,
    })
}

pub fn extract_attachment_payloads(raw: &[u8]) -> Result<Vec<AttachmentPayload>> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("message could not be parsed"))?;
    Ok(parsed
        .attachments()
        .map(|part| AttachmentPayload {
            content_type: part
                .content_type()
                .map(|kind| {
                    kind.c_subtype
                        .as_ref()
                        .map(|subtype| format!("{}/{}", kind.c_type, subtype))
                        .unwrap_or_else(|| kind.c_type.to_string())
                })
                .unwrap_or_else(|| "application/octet-stream".into()),
            content_id: part.content_id().map(str::to_string),
            bytes: part_bytes(part),
        })
        .collect())
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

fn part_bytes(part: &mail_parser::MessagePart<'_>) -> Vec<u8> {
    match &part.body {
        PartType::Text(value) => value.as_bytes().to_vec(),
        PartType::Html(value) => value.as_bytes().to_vec(),
        PartType::Binary(value) | PartType::InlineBinary(value) => value.to_vec(),
        PartType::Message(message) => message.raw_message().to_vec(),
        PartType::Multipart(_) => Vec::new(),
    }
}

/// The only HTML that crosses the IPC boundary is cleaned with a strict allowlist. Remote images
/// are omitted by default; inline `cid:` images can be resolved by the future attachment loader.
pub fn sanitize_html(input: &str) -> String {
    let mut allowed_schemes = HashSet::new();
    allowed_schemes.insert("cid");
    allowed_schemes.insert("https");
    Builder::default()
        .url_schemes(allowed_schemes)
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
}
