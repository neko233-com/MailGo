# MailGo production boundary

The repository is deliberately split into a real desktop UI foundation and provider/service boundaries that can grow without putting mail transport code in React components.

## Implemented baseline

1. `src/data.ts` defines provider-neutral account and message models plus the Google/QQ/Outlook quick-start registry.
2. `src/lib/ipc.ts` keeps browser development and native rdesktop calls on the same request/response contract.
3. `native/src/main.rs` owns durable metadata, schema versioning, atomic JSON replacement, and secret storage through the OS keyring.
4. The UI is local-first: a browser preview stays usable without native IPC, while native mode persists account metadata and preferences under `%LOCALAPPDATA%\\MailGo`.
5. Rich HTML is sanitized before rendering; scripts, frames, forms, event handlers, and JavaScript URLs are removed.
6. Rust provider profiles cover Google/Gmail, QQ, Outlook, and custom IMAP/SMTP endpoints. IMAP sync, lazy MIME parsing, SMTP send, UID flags, and local classification are wired behind IPC.
7. Windows mailbox caches are protected with DPAPI, account credentials use Windows Credential Manager, and metadata exports stay redacted.
8. Windows tray lifecycle, single-instance activation, crash-safe state recovery, and a five-minute background sync scheduler are implemented without blocking the rdesktop event loop.
9. Native OAuth2 + PKCE exchange and refresh-token rotation are available when registered client IDs are provided through environment configuration; codes and tokens remain outside metadata.
10. Provider folder mappings, DPAPI-protected attachment storage, inline CID image embedding, and an offline flag-mutation queue are implemented.
11. Bounded exponential retry is applied to transient IMAP synchronization failures while authentication/configuration failures fail fast.
12. Mailbox caches retain `UIDVALIDITY`, the oldest UID, and a continuation marker; `sync.page` fetches older UID ranges, merges them idempotently, and resets the affected cache when the server UID space changes.

## Remaining production acceptance work

- Device-flow UX, provider capability discovery, server rate-limit handling, and provider-specific incremental delta sync beyond the resumable UID-page path.
- Attachment download progress/cancellation and richer MIME regression fixtures; the current path is bounded and encrypted but intentionally returns a single IPC payload.
- Notification policy and tray integration tests on supported Windows versions.
- Encrypted, user-confirmed portable secret transfer if MailGo ever adds credential migration.

Production acceptance requires integration tests against disposable provider fixtures, a Windows WebView2 smoke test, migration tests for every persisted-state schema, and a security review of HTML/MIME parsing before shipping.
