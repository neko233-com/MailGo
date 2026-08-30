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
8. Windows tray lifecycle and a five-minute background sync scheduler are implemented without blocking the rdesktop event loop.

## Remaining production acceptance work

- OAuth authorization-code exchange with registered Google/Microsoft client IDs and redirect/device-flow handling.
- Resumable multi-folder sync (RFC 3501/6154), UIDVALIDITY migrations, backoff/rate-limit handling, and an offline mutation queue.
- Inline CID image resolution, attachment streaming/downloads, sent/drafts/spam/trash folder adapters, and richer MIME regression fixtures.
- Single-instance activation, notification policy, crash recovery, and tray integration tests on supported Windows versions.
- Encrypted, user-confirmed portable secret transfer if MailGo ever adds credential migration.

Production acceptance requires integration tests against disposable provider fixtures, a Windows WebView2 smoke test, migration tests for every persisted-state schema, and a security review of HTML/MIME parsing before shipping.
