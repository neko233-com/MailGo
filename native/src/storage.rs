use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_CACHE_FILES: u64 = 250_000;
const MAX_CACHE_DEPTH: usize = 32;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub total_bytes: u64,
    pub file_count: u64,
    pub mail_bytes: u64,
    pub attachment_bytes: u64,
    pub draft_bytes: u64,
    pub outbox_bytes: u64,
    pub operation_bytes: u64,
    pub other_bytes: u64,
    pub unreadable_entries: u64,
    pub truncated: bool,
    pub scanned_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheKind {
    Mail,
    Attachment,
    Draft,
    Outbox,
    Operation,
    Other,
}

/// Measure encrypted cache usage without following symlinks or allowing an unexpectedly large
/// cache tree to monopolize a background worker forever.
pub fn measure(cache_root: &Path) -> CacheStats {
    let mut stats = CacheStats::default();
    let mut pending = vec![(cache_root.to_path_buf(), 0usize)];

    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_CACHE_DEPTH {
            stats.truncated = true;
            continue;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && directory == cache_root =>
            {
                break;
            }
            Err(_) => {
                stats.unreadable_entries = stats.unreadable_entries.saturating_add(1);
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    stats.unreadable_entries = stats.unreadable_entries.saturating_add(1);
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    stats.unreadable_entries = stats.unreadable_entries.saturating_add(1);
                    continue;
                }
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                pending.push((entry.path(), depth.saturating_add(1)));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if stats.file_count >= MAX_CACHE_FILES {
                stats.truncated = true;
                pending.clear();
                break;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    stats.unreadable_entries = stats.unreadable_entries.saturating_add(1);
                    continue;
                }
            };
            let bytes = metadata.len();
            stats.file_count = stats.file_count.saturating_add(1);
            stats.total_bytes = stats.total_bytes.saturating_add(bytes);
            match classify(cache_root, &entry.path()) {
                CacheKind::Mail => stats.mail_bytes = stats.mail_bytes.saturating_add(bytes),
                CacheKind::Attachment => {
                    stats.attachment_bytes = stats.attachment_bytes.saturating_add(bytes)
                }
                CacheKind::Draft => stats.draft_bytes = stats.draft_bytes.saturating_add(bytes),
                CacheKind::Outbox => stats.outbox_bytes = stats.outbox_bytes.saturating_add(bytes),
                CacheKind::Operation => {
                    stats.operation_bytes = stats.operation_bytes.saturating_add(bytes)
                }
                CacheKind::Other => stats.other_bytes = stats.other_bytes.saturating_add(bytes),
            }
        }
    }

    stats.scanned_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    stats
}

fn classify(cache_root: &Path, path: &Path) -> CacheKind {
    let relative = path.strip_prefix(cache_root).unwrap_or(path);
    if relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value.eq_ignore_ascii_case("draft-attachments-v1"))
    }) {
        return CacheKind::Draft;
    }
    if relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value.eq_ignore_ascii_case("attachments"))
    }) {
        return CacheKind::Attachment;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if file_name.starts_with("drafts.bin") {
        CacheKind::Draft
    } else if file_name.starts_with("outbox.bin") {
        CacheKind::Outbox
    } else if file_name.starts_with("mutations.bin") || file_name.starts_with("moves.bin") {
        CacheKind::Operation
    } else if file_name.starts_with("inbox.bin")
        || file_name.starts_with("folder_")
        || file_name.starts_with("mail-index-v1.sqlite3")
        || file_name.starts_with("search-index-key-v1.bin")
    {
        CacheKind::Mail
    } else {
        CacheKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mailgo-storage-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn missing_cache_is_a_valid_empty_snapshot() {
        let root = test_root("missing");
        let stats = measure(&root);
        assert_eq!(stats.total_bytes, 0);
        assert_eq!(stats.file_count, 0);
        assert!(!stats.truncated);
        assert!(stats.scanned_at > 0);
    }

    #[test]
    fn cache_usage_is_classified_without_double_counting() {
        let root = test_root("classified");
        let account = root.join("account-1");
        let attachments = account.join("attachments").join("folder");
        fs::create_dir_all(&attachments).unwrap();
        fs::write(account.join("inbox.bin"), [0u8; 11]).unwrap();
        fs::write(account.join("folder_abcd.bin"), [0u8; 13]).unwrap();
        fs::write(root.join("mail-index-v1.sqlite3-wal"), [0u8; 7]).unwrap();
        fs::write(root.join("mail-index-v1.sqlite3.backup"), [0u8; 5]).unwrap();
        fs::write(root.join("search-index-key-v1.bin"), [0u8; 3]).unwrap();
        fs::write(attachments.join("1-0.bin"), [0u8; 17]).unwrap();
        fs::write(root.join("drafts.bin"), [0u8; 19]).unwrap();
        let draft_attachment = root
            .join("draft-attachments-v1")
            .join("account-hash")
            .join("draft-1");
        fs::create_dir_all(&draft_attachment).unwrap();
        fs::write(draft_attachment.join("attachment.bin"), [0u8; 9]).unwrap();
        fs::write(root.join("outbox.bin.bak"), [0u8; 23]).unwrap();
        fs::write(account.join("moves.bin"), [0u8; 29]).unwrap();
        fs::write(root.join("unknown.dat"), [0u8; 31]).unwrap();

        let stats = measure(&root);
        assert_eq!(stats.file_count, 11);
        assert_eq!(stats.mail_bytes, 39);
        assert_eq!(stats.attachment_bytes, 17);
        assert_eq!(stats.draft_bytes, 28);
        assert_eq!(stats.outbox_bytes, 23);
        assert_eq!(stats.operation_bytes, 29);
        assert_eq!(stats.other_bytes, 31);
        assert_eq!(stats.total_bytes, 167);
        assert_eq!(
            stats.total_bytes,
            stats.mail_bytes
                + stats.attachment_bytes
                + stats.draft_bytes
                + stats.outbox_bytes
                + stats.operation_bytes
                + stats.other_bytes
        );

        fs::remove_dir_all(root).unwrap();
    }
}
