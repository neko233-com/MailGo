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
10. Outlook Device Flow is available in the account assistant with verification-page launch, user-code display, bounded polling, expiry handling, and in-memory handoff to the Windows credential store.
11. Provider folder mappings, DPAPI-protected attachment storage, inline CID image embedding, and an offline flag-mutation queue are implemented.
12. Bounded exponential retry is applied to transient and rate-limited IMAP synchronization failures while authentication/configuration failures fail fast.
13. Mailbox caches retain `UIDVALIDITY`, the oldest UID, and a continuation marker; `sync.page` fetches older UID ranges, merges them idempotently, and resets the affected cache when the server UID space changes.
14. Background sync can emit a Windows tray notification for newly observed unread mail, with a persisted user-controlled notification policy.
15. Attachment downloads use a bounded chunked IPC protocol with explicit start/chunk/cancel commands; encrypted cache bytes never cross the WebView in one unbounded payload.
16. Warm IMAP syncs fetch only UIDs newer than the cached high-water mark, refresh a bounded recent flag window, preserve paged history, and remove messages deleted remotely; UIDVALIDITY changes still force a safe cache reset.
17. Batch selection, mark-read, archive, and delete actions use UID-based native IPC; folder mutations update encrypted local caches immediately and replay through a separate encrypted queue when the provider is unavailable. Gmail archive removes the Inbox label through `X-GM-LABELS`.
18. Compose attachments use bounded chunked upload IPC with per-file/total/count limits, expiry and cancellation; Rust builds multipart MIME messages without persisting outgoing file bytes.
19. Remote HTML images are blocked by default in the renderer, while CID/data images remain available; the preference is persisted in native metadata and can be explicitly enabled with a privacy warning.
20. Release packaging has a reproducible Windows portable ZIP path; the native shell resolves renderer assets beside the executable, with an environment override for controlled deployment.
21. Existing accounts can re-enter the authorization flow without changing their stable account ID, and native account removal clears the protected credential plus the account-scoped offline cache.

## Remaining production acceptance work

- Provider-specific incremental delta sync beyond the resumable UID-page path.
- Provider-specific rate-limit headers/backoff and richer MIME regression fixtures.
- Tray integration tests on supported Windows versions.
- Signed installer generation on a trusted Windows release host; the current rdesktop NSIS command is still an upstream placeholder.
- Encrypted, user-confirmed portable secret transfer if MailGo ever adds credential migration.

Production acceptance requires integration tests against disposable provider fixtures, a Windows WebView2 smoke test, migration tests for every persisted-state schema, and a security review of HTML/MIME parsing before shipping.
