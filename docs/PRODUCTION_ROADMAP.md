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
22. Windows desktop can export/import fully configured accounts through a password-protected Argon2id + ChaCha20-Poly1305 bundle; decrypted credentials are written only to Windows Credential Manager and imported account caches are reset before the next sync.
23. The per-user rdesktop updater preserves a working installation when local application-control policy blocks source builds and only accepts a checksum-verified official binary fallback when it is newer.
24. Persisted state loading explicitly migrates legacy missing fields and both snake_case/camelCase spellings, normalizes the current schema version, and rejects future unsupported versions before touching the backup state.
25. Native MIME sanitization now retains only safe HTTPS image sources and removes tracking-related attributes before caching; the renderer blocks remote images by default while preserving safe HTTPS/mailto links and inline CID images.
26. Sync retry handling honors numeric `Retry-After` hints that survive transport errors, with a bounded 1–300 second cap and exponential fallback for providers that omit the hint.
27. Offline flag and move queues are covered by executable regression tests for coalescing semantics and immediate encrypted-cache consistency; temporary test state is process-scoped and cleaned up after each case.
28. The native queue-status IPC exposes only encrypted-queue counts, and the desktop cache footer reports pending local operations across all configured accounts after mutations and sync.
29. Account onboarding distinguishes credential-save failure from first-sync failure, preserving a successfully stored account as offline/reauthorization-needed instead of rolling back only the renderer state.
30. Full MIME parsing is bounded to 64 MiB, attachments are capped by count and aggregate size, and untrusted attachment names are normalized before they enter cache metadata or download flows.
31. Native attachment upload sessions are cleared after every send attempt, and OAuth pending/callback secrets are zeroized when their in-memory session values are released.
32. Native IPC validates bounded recipient, subject, body, HTML, and manual-credential fields before any SMTP, OAuth, or keyring operation.
33. Credential, configuration, and IMAP failures now persist an offline or reauthorization-needed account status across manual sync, first sync, background sync, and restart; the desktop renderer refreshes those statuses while the tray scheduler continues running.
34. Advertising classification now has a persisted user-controlled suppression mode: normal lists can hide classified ads while Apple Connect security mail remains visible and Apple/other advertising remains reachable through smart categories.

35. Outgoing mail now accepts bounded To/CC/BCC recipient lists and can send an escaped HTML alternative alongside the plain-text body; native recipient parsing rejects empty or malformed addresses before SMTP transport.

36. Cache-reading IPC now verifies account ownership, account IDs reject filesystem traversal segments, custom IMAP/SMTP hosts reject control characters and path separators, and mailbox cache reads validate folder names before deriving paths.

37. Native mode stores up to 100 bounded per-account drafts in a DPAPI-protected cache, restores the most recent draft when composing, debounces saves, and removes the draft after a successful send; attachments remain session-scoped by design.
38. Native mode aggregates encrypted local drafts into the desktop 草稿箱 view, keeps its count current, opens the selected draft by ID, and exposes an explicit discard action so multiple accounts cannot restore or delete the wrong draft.
39. IMAP sync now adopts all bounded selectable mailboxes returned by `LIST`, persists a sanitized per-account folder index, and lets the renderer browse custom server folders offline with account-scoped UID pagination.
40. Reply, reply-all, and forward now carry the active account context, preserve parsed To/CC recipients, add idempotent subject prefixes, and quote bounded original content while keeping unrelated local drafts isolated.
41. The documented desktop shortcuts now match the implementation: compose, reply, search focus, and transient UI dismissal are keyboard-accessible.
42. Transient send failures now enter a bounded DPAPI-protected offline outbox without storing provider credentials; automatic retry uses safe backoff, permanent/authentication failures pause, and the desktop exposes status and manual retry controls.
43. MIME text/HTML and CID expansion, IMAP header payloads, UID discovery, mailbox caches, Base64 upload chunks, and import files now have explicit pre-allocation or post-expansion bounds; cache mutations are UIDVALIDITY-bound and cache writes are serialized.

## Remaining production acceptance work

- Provider-specific incremental delta sync beyond the resumable UID-page path.
- Provider-specific rate-limit headers/backoff and richer MIME regression fixtures.
- Disposable-provider acceptance tests for OAuth/IMAP/SMTP, including reconnect, UIDVALIDITY changes, folder mappings, and server-side mutation conflicts.
- Tray integration tests on supported Windows versions.
- Signed installer generation on a trusted Windows release host; the current rdesktop NSIS command is still an upstream placeholder.
- Independent authentication of the floating rdesktop updater trust root, packaged IPC caller isolation, and a protected non-Windows cache backend.

Production acceptance requires integration tests against disposable provider fixtures, a Windows WebView2 smoke test, migration tests for every persisted-state schema, and a security review of HTML/MIME parsing before shipping.
