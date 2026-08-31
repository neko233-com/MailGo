# MailGo production boundary

The repository is deliberately split into a real desktop UI foundation and provider/service boundaries that can grow without putting mail transport code in React components.

## Implemented baseline

1. `src/data.ts` defines provider-neutral account and message models plus the Google/QQ/Outlook quick-start registry.
2. `src/lib/ipc.ts` keeps browser development and native rdesktop calls on the same request/response contract.
3. `native/src/main.rs` owns durable metadata, schema versioning, atomic JSON replacement, and secret storage through the OS keyring.
4. The UI is local-first: a browser preview stays usable without native IPC, while native mode persists account metadata and preferences under `%LOCALAPPDATA%\\MailGo`.
5. Rich HTML is sanitized before rendering; scripts, frames, forms, event handlers, and JavaScript URLs are removed.
6. Rust provider profiles cover Google/Gmail, QQ, Outlook, and custom IMAP/SMTP endpoints. IMAP sync, lazy MIME parsing, SMTP send, UID flags, and local classification are wired behind IPC.
7. Windows mailbox caches are protected with DPAPI, account credentials use Windows Credential Manager, and normal metadata exports stay redacted; encrypted transfer bundles are the explicit credential-bearing exception.
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
23. The per-user rdesktop updater installs only the manually reviewed immutable SHA in `config/rdesktop-trusted-revision.txt`, preserves a working installation when local application-control policy blocks source builds, and only accepts a release fallback when a configured Authenticode signer trust root and the asset digest both verify.
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
44. Search now keeps local filtering instant while a debounced native IMAP query searches the full discovered folder set across selected or all accounts; bounded header hits are merged into UIDVALIDITY-aware encrypted caches so they remain actionable offline.
45. Network transport now has explicit deadlines: IMAP uses bounded address connection attempts plus TLS/STARTTLS and socket read/write timeouts, OAuth HTTP requests use bounded connect/read/write timeouts, and SMTP delivery has a bounded transport timeout for predictable background/offline behavior.
46. OAuth loopback callbacks now use a bounded shared listener per configured port/path and route successful or failed returns by validated `state`, so simultaneous Google/Outlook authorization flows do not compete for one-shot `accept` ownership.
47. Account onboarding and import enforce a shared 64-account ceiling, while pending OAuth sessions are capped and expired sessions are purged before new flows start.
48. IMAP synchronization now enables capability-driven QRESYNC/CONDSTORE deltas, persists bounded `HIGHESTMODSEQ` cursors, removes QRESYNC `VANISHED` UIDs, discovers new UIDs without an `ALL` search, and safely falls back to the existing UID path when an extension is unavailable or rejected.
49. Non-Windows builds now protect mailbox and attachment caches with a random XChaCha20-Poly1305 key stored in the platform keyring; the Windows DPAPI path remains unchanged.
50. OAuth device, authorization-code, and refresh requests now retain bounded numeric `Retry-After` guidance for HTTP 429 responses without exposing response bodies or credentials.
51. Incremental and UID fallback syncs retain the previous delta cursor while bounded header windows are incomplete, fetch unseen UIDs oldest-first, and only advance the cursor after every requested header parses successfully.
52. Packaged native RPC now requires a per-launch capability carried by the trusted app URL, while the renderer also ships a restrictive CSP that blocks executable HTML, frames, plugins, and unapproved network destinations.
53. Narrow Windows windows now switch between a single-column mail list and reading view, with drawer navigation, drawer-based authorization help, title-bar compose access, and full-screen compose bounds instead of allowing the four-pane desktop grid to overflow the viewport.
54. Theme changes now persist through the native state after hydration, and user CSS is bounded to 64 KiB with a storage-failure fallback so customization cannot grow without limit or make the settings surface unusable.
55. Browser/WebView preference reads and writes now tolerate unavailable local storage, and global compose/reply shortcuts ignore inputs, selects, textareas, and contenteditable controls without assuming every keyboard event target is an HTML element.
56. Compose now supports bounded image selection with local previews and generated safe CID references; native SMTP builds `multipart/related` HTML mail, while retryable sends preserve inline resources in the encrypted outbox.
57. Medium desktop widths now move the authorization assistant into its existing drawer boundary and relax the reading-column minimum, eliminating horizontal overflow at 1280px and tighter laptop widths while keeping the four-pane large-screen layout.
58. The desktop shell no longer imposes a 960px body minimum, so the responsive grid can honor the actual viewport before switching to the single-column mobile layout.
59. Attachment upload metadata now rejects NUL and other unsafe control characters at the native IPC boundary, with regression coverage for file names, MIME types, and inline Content-ID values.
60. Offline-only mode is now an enforced local-first policy: it pauses background/manual network sync and remote search, keeps cached mail readable, queues outgoing mail in the encrypted outbox without opening a network connection, and queues flag/move mutations for replay after the mode is disabled.
61. The Windows release gate now performs build, format, clippy, Release-test, portable-package, SHA-256, and manifest checks; GitHub publication remains opt-in behind an explicit tag and `-Publish` flag with no automated workflow.
62. Windows ZIP packaging now writes entries in stable lexical order with a fixed DOS-compatible timestamp, making repeated local builds byte-identical when their inputs are unchanged.
63. The packaged renderer no longer depends on remote web fonts, and the Windows packaging gate rejects Google Fonts references so offline startup does not create an implicit font-network dependency.
64. HTML rendering now suppresses HTTPS image requests while only-offline mode is active, even when the persistent remote-image preference is enabled; same-message CID images remain available.
65. The Windows tray window listens for the system `TaskbarCreated` broadcast and re-adds the MailGo icon after Explorer restarts; failed notification updates also attempt a safe re-registration.
66. Encrypted draft read-modify-write operations now share a process-wide lock, with concurrent-save regression coverage to prevent autosave races from corrupting the draft store.
67. Encrypted account import now checks the combined post-replacement account count before writing credentials, preserving the shared 64-account ceiling transactionally.
68. Redacted account import now preflights and de-duplicates records, performs cache/credential cleanup before state mutation, and restores the prior account state and credentials if cleanup or persistence fails.
69. Redacted import distinguishes an absent credential (normal reauthorization) from an unavailable Credential Manager, failing closed before committing state in the latter case.
70. Account reauthorization now snapshots and restores the prior state/credential when persistence fails, while committed accounts remain usable if local outbox resumption needs a later retry.
71. Account removal now snapshots the prior account state and credential, aborts before state mutation when cleanup fails, and restores both when final persistence fails.
72. Credential reads, OAuth refresh results, transfer records, and rollback snapshots now use zeroizing wrappers so secret-bearing intermediate values are cleared on drop, including encrypted transfer serialization.
73. Windows authorization and provider-help links now use a capability-gated native HTTPS/`mailto` default-handler opener; browser previews retain `window.open`, while embedded credentials and unrelated schemes are rejected.
74. Sanitized HTML mail links now delegate HTTPS and `mailto` navigation to the same native external opener, preventing WebView navigation away from MailGo and rejecting unsafe schemes at the event boundary.
75. Native startup now begins with an empty account/cache view instead of browser-demo data; the first-run empty state distinguishes local-state loading, no configured accounts, and an empty mailbox, with a direct add-account action.
76. Google and Outlook OAuth redirect configuration now fails closed unless it is an explicit `http://127.0.0.1:<port>/<path>` callback without userinfo, query, or fragment components; this keeps authorization returns local to the desktop process.
77. The native scheduler performs one delayed startup sync before entering its five-minute background cadence, so a resumed desktop refreshes stale cached headers promptly while offline-only mode still prevents all network work.
78. User CSS is bounded, persisted only after sanitization, and cannot load `@import`, `url()`, script protocols, legacy behavior properties, CSS comments, or escape-obfuscated equivalents; theme variables, layout rules, gradients, media queries, and animations remain supported.
79. Mail actions now expose a provider-mapped “move to spam” command for single messages and batches; it reuses the UID-based native move path, encrypted offline mutation queue, UIDVALIDITY checks, and immediate local cache updates for Gmail, QQ, Outlook, and custom accounts.
80. Messages can also be restored to `INBOX` from archive, spam, trash, or custom folders through a dedicated native-validated command, with the same offline replay and immediate cache semantics.
81. The per-user rdesktop updater now places Cargo build artifacts under the already validated `%LOCALAPPDATA%\MailGo\cargo-target` root and installs only the checked-in reviewed rdesktop revision, avoiding temporary-directory application-control failures and moving-default-branch supply-chain drift.
82. Native users can move a message from the reading menu into any discovered server folder, including provider-mapped system folders; the target list is deduplicated and current-folder aware, while the existing UID-based offline queue handles replay.
83. The documented native smoke command now launches the Release shell, while the development build remains available separately; this avoids Windows application-control policies rejecting the Debug binary during packaged-startup verification.
84. Local smart classification now recognizes official App Store Connect, Developer, TestFlight, Apple Ads, and Search Ads subdomains before falling back to Apple-specific subject signals; security notices remain higher priority and advertising classification stays opt-in to hiding.
85. Closing or switching away from account authorization now cancels native OAuth/device sessions and clears the renderer's authorization-code input, including ready device-flow credentials, instead of retaining abandoned temporary secrets until TTL cleanup.
86. Renderer IPC requests now clear their timeout after a response and clean up both pending state and timers when native `postMessage` fails; request IDs use a cryptographic UUID when the runtime provides one, improving long-lived background stability.
87. Account onboarding now uses a cryptographic UUID-based account ID for new accounts and guards the asynchronous save/sync path against duplicate submissions; the account dialog exposes a busy label and cannot close mid-commit.
88. MIME parsing now rejects messages exceeding the attachment-count limit before collecting attachment metadata, with regression coverage for oversized multipart attachment sets; this keeps rich HTML/inline-image support bounded against multi-part abuse.
89. Provider email validation now rejects controls, whitespace, duplicate separators, malformed domain labels, and ambiguous local parts before credentials or transport are opened.
90. Account reauthorization now keeps the mailbox identity immutable for a stable account ID; provider, email, and custom transport changes must use a new account, while case-variant IDs are rejected across state, import, and onboarding.
91. Non-INBOX mailbox caches and attachment directories now use SHA-256 folder keys, with legacy cache reads retained for migration and regression coverage proving lossy-name collisions cannot share new storage.
92. Full-message reads now preflight the server-advertised `RFC822.SIZE` before requesting the MIME body, while retaining the post-fetch 64 MiB bound as a defense against inaccurate servers.
93. The scheduled rdesktop updater now installs only the manually reviewed SHA in `config/rdesktop-trusted-revision.txt`; upstream default-branch movement cannot silently change the local framework.
94. Manual sync, pagination, server search, background refresh, and account reauthorization/removal now coordinate through a per-account in-flight lease, preventing duplicate IMAP work and cache recreation after destructive account operations.
95. A fail-closed MSIX packaging path now reuses the deterministic Release portable build, emits an explicit desktop manifest, requires Windows SDK tooling and a production certificate, and verifies the Authenticode signature before handing off the installer.
96. IMAP folder discovery now retains only a bounded selectable-folder index while preserving provider-preferred folders; the underlying IMAP library response buffer remains a release-host integration limitation for UID result sets.
97. The portable deployment script now verifies the release manifest, rejects ZIP traversal and oversized extraction, checks WebView2, deploys through a temporary directory, creates shortcuts, and retains the prior install for rollback; signed MSIX remains the production distribution path.
98. The OAuth loopback listener now reads a bounded complete HTTP header instead of assuming one TCP read contains the browser request, with regression coverage for split callback packets.
99. Desktop widths from 1100–1400px now reserve the authorization assistant as a fourth column, keeping reading actions such as rich HTML rendering reachable while narrower layouts retain drawer behavior.
100. Online move, archive, delete, read, and star actions now enqueue only classified transient transport/rate-limit failures; authentication and permanent provider errors return immediately, while optimistic renderer state rolls back and marks affected accounts for reauthorization.
101. Server-wide IMAP search and full-message downloads now use bounded read-only retries with the same transport/rate-limit backoff as mailbox synchronization; mutating commands remain single-attempt to avoid replaying a command after an ambiguous timeout.

## Remaining production acceptance work

- Provider-specific IMAP/SMTP rate-limit headers/backoff and broader provider MIME corpus coverage.
- Disposable-provider acceptance tests for OAuth/IMAP/SMTP, including reconnect, UIDVALIDITY changes, folder mappings, and server-side mutation conflicts.
- Tray integration tests on supported Windows versions.
- Signed installer generation on a trusted Windows release host; the current rdesktop NSIS command is still an upstream placeholder.
- Packaged IPC caller isolation beyond the renderer build guard and cross-target acceptance of the protected non-Windows cache backend on native dependency hosts; the updater remains dependent on a manually reviewed source pin or independently configured Authenticode trust root.

Production acceptance requires integration tests against disposable provider fixtures, a Windows WebView2 smoke test, migration tests for every persisted-state schema, and a security review of HTML/MIME parsing before shipping.
