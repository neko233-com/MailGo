# MailGo production boundary

The repository is deliberately split into a real desktop UI foundation and provider/service boundaries that can grow without putting mail transport code in React components.

## Implemented baseline

1. `src/data.ts` defines provider-neutral account and message models plus the Google/QQ/Outlook quick-start registry.
2. `src/lib/ipc.ts` keeps browser development and native rdesktop calls on the same request/response contract.
3. `native/src/main.rs` owns durable metadata, schema versioning, atomic JSON replacement, and secret storage through the OS keyring.
4. The UI is local-first: a browser preview stays usable without native IPC, while native mode persists account metadata and preferences under `%LOCALAPPDATA%\\MailGo`.
5. Rich HTML is sanitized before rendering; scripts, frames, forms, event handlers, and JavaScript URLs are removed.

## Next service modules

- `native/src/providers/`: provider adapters for OAuth where supported and IMAP/SMTP with provider-specific defaults.
- `native/src/sync/`: resumable sync jobs, RFC 3501/6154 folder mapping, UIDVALIDITY tracking, backoff, rate-limit handling, and offline mutation queue.
- `native/src/mail/`: MIME parsing, inline CID images, attachment streaming, safe HTML sanitization, and content-policy enforcement.
- `native/src/tray/`: tray icon, hide/show, deliberate quit, notification policy, and single-instance activation.
- `native/src/export/`: encrypted, user-confirmed backup format for credentials when a future version adds a portable secret transfer flow.

Production acceptance requires integration tests against disposable provider fixtures, a Windows WebView2 smoke test, migration tests for every persisted-state schema, and a security review of HTML/MIME parsing before shipping.
