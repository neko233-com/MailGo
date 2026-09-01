# MailGo

MailGo is a Windows-first local-first email workspace for multiple Google, QQ, Outlook, and custom IMAP/SMTP accounts.

The current foundation includes:

- React + Vite desktop UI with three-pane unified inbox.
- Resizable Windows windows stay usable below the desktop four-pane breakpoint: mail list and reading view become a single-column flow, with drawer navigation, authorization help, and a title-bar compose entry.
- `motion` transitions with `prefers-reduced-motion` support.
- Tree-shakeable `reicon-react` icon components.
- Multiple accounts, including multiple QQ accounts, account switching, and import/export with redacted credentials.
- Windows desktop also supports password-protected account migration: encrypted export/import keeps provider credentials inside an Argon2id + ChaCha20-Poly1305 bundle and writes them back only to Windows Credential Manager.
- Local-first UI state with offline cache indicators and a Rust IPC boundary for durable state.
- Native startup enters the mailbox as soon as local state is available; cache hydration, per-account synchronization, and queue telemetry continue independently in the background with local loading/error states instead of a full-window blocking spinner.
- The message list is virtualized and renders only its visible window, while older cached or remote messages load in bounded pages from the list edge without rebuilding the whole mailbox view.
- The encrypted row-oriented SQLite mail index uses WAL transactions and UID indexes for bounded page/exact-message reads. Completed background synchronizations create a rate-limited online recovery copy; confirmed database corruption is quarantined and restored from an integrity-checked backup, while lock, permission, and disk errors are never misclassified as corruption.
- Server-search hits now enter that index through bounded UID upserts instead of decrypting and replacing a whole folder. Optimistic archive/move operations decrypt one source row and commit the source delete, target insert, both metadata revisions, and target retention cap in one SQLite transaction; pre-index encrypted snapshots retain a one-time migration fallback.
- Search opens from the complete local SQLite cache first—even offline—then adds remote IMAP matches independently. The local index stores only keyed HMAC bigram/trigram tokens under a per-installation OS-protected key, decrypts and verifies a bounded candidate set to eliminate blind-index false positives, and rebuilds incrementally on a native background thread without delaying the mailbox window.
- RFC `Message-ID`, `In-Reply-To`, and `References` metadata is parsed through bounded native fields and grouped with a folder/account-scoped reply graph. The virtual list renders one row per conversation with participant, unread, and message counts; selecting a conversation maps back to its individual messages, and the reading view navigates the oldest-to-newest chain while lazily fetching only the opened body. Reply and reply-all compose flows emit bounded RFC reply headers, preserving them through encrypted drafts and the offline outbox so online delivery and later retries stay in the same provider conversation.
- The native shell starts from the real local account/cache state; demo messages are browser-preview-only, and first launch provides a direct add-account path.
- Optional offline-only mode is enforced by the native boundary: cached mail remains readable, network sync/search are paused, outgoing mail is queued encrypted, and local flag/move mutations replay when online mode is restored.
- Provider quick links and guided authorization-code onboarding.
- Safe HTML preview mode, attachments, smart categories (including Apple Connect and Apple advertising), search, unread filter, star, reply, compose, theme switching, and user CSS overrides.
- Local-only advertising classification can optionally hide ads from normal lists while keeping Apple Connect security mail and smart-category access visible.
- Rust `neko233-com/rdesktop` WebView2 shell with custom frameless title bar and preserved WebView data directory under `%LOCALAPPDATA%\\MailGo\\WebView2`.
- The renderer uses Windows system fonts rather than remote web fonts, so the packaged UI has no implicit font-network dependency in offline mode.
- Windows Credential Manager integration through `keyring` for authorization-code storage; secrets never enter `state.json` or redacted account exports. Encrypted transfer bundles are the explicit exception and require a user-chosen passphrase.
- Theme changes are written to the native state after startup hydration, and user CSS is capped at 64 KiB with a session-only fallback when WebView storage is unavailable.
- Packaged native RPC calls carry a per-launch capability from the trusted app URL; forged or navigated-away renderer messages are rejected before any account, credential, filesystem, or network operation.
- Native IMAP sync uses capability-driven `QRESYNC`/`CONDSTORE` deltas when a server exposes them (including `HIGHESTMODSEQ` cursors, `VANISHED` deletions, and UID-only new-mail discovery), with a UID-based incremental header fallback across provider folder mappings. Bounded `UID FLAGS` refreshes, lazy full-message retrieval, replayable offline flag mutations, local flag updates, and protected mailbox/attachment caches keep the offline view safe: Windows uses DPAPI, while non-Windows builds use an OS keyring-held XChaCha20-Poly1305 key. Attachment downloads use bounded start/chunk/cancel IPC with progress and cancellation support.
- Every multi-item IMAP `FETCH` uses the RFC-required parenthesized data-item list. This is regression-tested and was verified with a live QQ mailbox on Windows, where the previously permissive-only query form was rejected with `BAD` before any headers could be cached.
- Native sync discovers selectable IMAP mailboxes (including custom folders), persists a bounded per-account folder index, and exposes those folders in the desktop sidebar for offline browsing and UID pagination.
- IMAP Modified UTF-7 mailbox names are decoded into Unicode display labels at the native boundary while their original wire names remain untouched for select, search, pagination, and move commands.
- Search combines the local cache's immediate results with a debounced, bounded native IMAP search across all discovered folders and accounts; server hits are merged into the encrypted cache with their UIDVALIDITY context before they reach the renderer.
- Network operations fail predictably: IMAP resolves/connects through bounded socket attempts with TLS/STARTTLS and read/write deadlines, OAuth HTTP calls use bounded connect/read/write timeouts and preserve bounded numeric `Retry-After` guidance for HTTP 429 responses, and SMTP sends have a bounded transport timeout so background sync and the offline outbox cannot hang indefinitely.
- Rolling local logs retain 14 days of startup and synchronization lifecycle events. IMAP failures are persisted as privacy-safe categories and protocol variants such as `network`/`imap-bad`, never as provider response text, addresses, or credentials.
- Mail actions use UID semantics end-to-end: batch selection supports mark-read, archive, move-to-spam, restore-to-inbox, and delete; archive/spam/trash/restore operations update encrypted local folder caches immediately and queue provider mutations when offline. Gmail archive removes the Inbox label instead of copying the message into a duplicate folder.
- The reading view also exposes discovered server folders as move destinations; custom folders are deduplicated and the current folder is omitted, while offline moves remain encrypted and replayable.
- The desktop cache footer reports the number of encrypted flag/move mutations still waiting for provider replay across all accounts; the renderer receives counts only, never queue contents.
- The cache footer no longer presents sample quota data: a bounded native worker measures the real encrypted cache asynchronously, skips symlinks, caps traversal, and reports aggregate mail/attachment/draft/queue bytes without exposing file paths. The renderer shows an isolated loading shimmer and refreshes the snapshot without blocking account or mailbox hydration.
- Native mode also has an encrypted, bounded offline 发件箱: transient SMTP/network failures are queued without credentials, retried with bounded backoff, paused after repeated permanent failures, and resumed after reauthorization.
- Native SMTP sending supports plain text and HTML alternatives through provider-specific TLS/STARTTLS defaults.
- Compose supports bounded To/CC/BCC recipient lists, safe HTML alternatives, and chunked multi-file attachments without putting attachment bytes in a single IPC request.
- Reply, reply-all, and forward preserve the active account, prefill recipients from MIME To/CC headers, add safe subject prefixes, and quote the original message without restoring an unrelated draft.
- The desktop keyboard flow includes `C` for compose, `R` for replying to the selected message, `Ctrl/Cmd+K` for search, and `Esc` for closing transient UI.
- Native mode automatically saves and restores per-account drafts in a DPAPI-protected local store. Draft attachments and inline images are encrypted as independent bounded files, restored without blocking the text editor, reused directly by native send/outbox paths, and removed after send or explicit discard.
- Mailbox caches and MIME payloads have explicit byte/count limits; cached mutations are bound to UIDVALIDITY, and cache/outbox writes are serialized to avoid scheduler/IPC races.
- MIME parsing enables the parser's complete legacy-charset support, including common GB2312/GBK/GB18030, Big5, and Japanese encodings. Cache schema v3 reparses derived header and conversation metadata only after a successful remote sync, so failed migrations leave the prior offline snapshot intact.
- Native mode also surfaces those encrypted local drafts in the 草稿箱 list, with per-account counts, draft-specific continue-editing actions, and an explicit discard action.
- Draft persistence is serialized across concurrent compose autosaves, preventing two windows from corrupting or losing each other's encrypted draft store.
- Encrypted account imports validate the combined account count before touching Credential Manager, so replacing existing accounts cannot silently exceed the 64-account ceiling.
- Redacted account imports preflight every record and commit the state change only after cache and credential cleanup succeeds; a failed import restores the prior in-memory state and credentials.
- Missing credentials are treated as an expected reauthorization state during redacted import, while unexpected Credential Manager failures abort the import before state is committed.
- Account reauthorization now commits the new credential and metadata as one recoverable operation; a persistence failure restores the previous account state and credential, and outbox-resume errors cannot falsely report the account as unsaved.
- Account removal uses the same recovery boundary: cache, draft, outbox, and Credential Manager cleanup must finish before the account list is committed, and failures restore the prior credential/state snapshot.
- Windows tray lifecycle is implemented with the generated `resources/icons/mailgo.ico`: close-to-tray, restore on click, deliberate quit, and a five-minute background sync scheduler.
- The title bar, WebView favicon, native window, executable resource, tray, and installed shortcuts use the same MailGo artwork. Windows packaging rejects malformed ICO files or files missing the required 16/24/32/48/64/128/256-pixel 32-bit entries, and shortcuts reference the packaged multi-size ICO directly.
- The packaged Windows Release uses the GUI subsystem and does not open a companion console window. Close-to-tray, same-process single-instance restore, and completion of an in-flight sync while hidden have been accepted on the local Windows build.
- The background scheduler performs a short delayed first sync after launch, then continues on its five-minute cadence; offline-only mode skips both paths.
- Accounts whose servers advertise IMAP IDLE also keep a dedicated bounded listener for immediate Inbox change wakeups. Listeners re-check offline/account lifecycle state every 30 seconds, retain the native 60-second socket ceiling even when the upstream IDLE handle resets its timeout, reconnect with capped exponential backoff, and hand actual synchronization to the same per-account lease; the five-minute scheduler remains the compatibility fallback.
- The tray icon re-registers itself after Windows Explorer/taskbar restarts, so a hidden MailGo window remains recoverable without restarting the app.
- Custom IMAP/SMTP onboarding accepts host, port, TLS mode, and password/app-password/OAuth2 settings without putting credentials in metadata.

Provider authentication is deliberately explicit: Gmail defaults to a provider-issued app password and can use native OAuth2 when a registered client is configured; QQ uses its provider-issued authorization code; Outlook uses OAuth2 Device Flow or loopback PKCE; custom OAuth2 accounts can use a provider-issued Bearer access token. Set `MAILGO_GOOGLE_CLIENT_ID` or `MAILGO_OUTLOOK_CLIENT_ID` (and an optional redirect URI/client secret) to enable the native OAuth2 flow. The app never persists the one-time code itself.

For a registered desktop OAuth client, configure the client before launching the native shell:

```powershell
$env:MAILGO_GOOGLE_CLIENT_ID = "your-registered-google-client-id"
$env:MAILGO_GOOGLE_REDIRECT_URI = "http://127.0.0.1:8765/oauth/callback"
$env:MAILGO_OUTLOOK_CLIENT_ID = "your-registered-microsoft-client-id"
$env:MAILGO_OUTLOOK_REDIRECT_URI = "http://127.0.0.1:8765/oauth/callback"
```

The redirect URI must be registered exactly with the provider. MailGo keeps a shared listener for each configured `127.0.0.1` callback port/path and routes simultaneous OAuth returns by their validated `state`; the account assistant exchanges the returned code directly, while manual code entry remains available when the callback port is unavailable. Outlook uses a native Device Flow path in the account assistant: it opens the verification page, displays the user code, polls with provider-supplied intervals, and keeps the resulting token only in Windows Credential Manager.

For safety, configured Google and Outlook redirect URIs must remain explicit loopback HTTP callbacks such as `http://127.0.0.1:8765/oauth/callback`; MailGo rejects remote hosts, embedded credentials, query strings, and fragments.

The per-user `MailGo-rdesktop-updater` scheduled task installs only the manually reviewed immutable commit recorded in `config/rdesktop-trusted-revision.txt`, through the already validated `%LOCALAPPDATA%\MailGo\cargo-target` Cargo directory. It never follows the upstream default branch automatically; maintainers must review and bump the pin deliberately.

Use the settings panel's encrypted account transfer actions when moving fully configured accounts between Windows machines. Choose a strong transfer password of at least 12 characters; the password is never stored, and a bundle cannot be recovered if it is forgotten. The browser preview intentionally disables credential-bearing transfer actions.

## Run the browser development surface

```powershell
npm install
npm run dev
```

For rdesktop Agent-first development, use the installed CLI (kept on the reviewed revision by the local updater):

```powershell
rdesktop dev --path .
```

The per-user `MailGo-rdesktop-updater` task runs weekly but installs only the reviewed SHA in `config/rdesktop-trusted-revision.txt`. If Windows application control blocks Cargo build scripts, it only accepts an official release when `MAILGO_RDESKTOP_SIGNER_THUMBPRINT` is configured and both the SHA-256 digest and Authenticode signer thumbprint match. Without that independent signer trust root, it preserves the current installation.

## Build the Windows shell

```powershell
npm run native:build
npm run native:run
```

The native shell loads `dist/` through the framework-owned `rdesktop://` protocol. Run `npm run build` before compiling Rust.
`npm run native:run` intentionally launches the Release shell so Windows application-control policies do not reject the Debug binary during smoke testing; `npm run native:build` remains the fast development build.

Create a self-contained portable Windows package with the release shell and renderer assets:

```powershell
npm run package:windows
# artifacts\MailGo-0.1.0-windows-x64.zip
```

Extract the archive and launch `MailGo.exe`. The package expects a compatible WebView2 runtime on Windows. The installed rdesktop 0.1.8 CLI currently emits a small NSIS placeholder from `rdesktop bundle`; do not distribute that generated file as an installer. Generate the portable archive on a release build host where the Windows application-control policy permits Cargo Release build scripts; this repository intentionally does not ship an unsigned or placeholder installer.
The Windows npm scripts place Cargo's target directory under `%LOCALAPPDATA%\MailGo\cargo-target` so application-control policies do not block dependency build scripts in the repository checkout.

For a user-level portable installation, pass the release ZIP and its adjacent manifest to the recoverable installer. It verifies the manifest hash, rejects unsafe/oversized archives, checks WebView2, creates a Start Menu shortcut, and keeps the previous install as a backup:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/install-portable.ps1 `
  -ArchivePath artifacts\MailGo-0.1.0-windows-x64.zip `
  -ManifestPath artifacts\MailGo-0.1.0-windows-x64.manifest.json
```

Build a signed MSIX on a Windows release host with the Windows SDK (`makeappx.exe` and `signtool.exe`) and a trusted production certificate. The command fails closed without those tools and a certificate; it does not create an unsigned production installer:

```powershell
$env:MAILGO_SIGNING_PFX_PASSWORD = "use-a-secret-manager-value"
npm run package:msix -- -Publisher "CN=MailGo Release" -CertificatePath C:\secure\MailGo.pfx
```

Prefer `-CertificateThumbprint` when the certificate is installed in the release host's protected certificate store. MSIX still requires the WebView2 Evergreen Runtime on the target machine; runtime bootstrapper packaging and signed-certificate provisioning belong to the release host, not the source repository.

## Publishing

This repository has no GitHub Actions. Releases and publication are intentionally manual and require an explicit user instruction. Run the release gate to build, test, package, hash, and write a manifest without publishing:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release-windows.ps1
```

Only an explicit tag plus `-Publish` calls the GitHub CLI:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release-windows.ps1 -Publish -Tag v0.1.0
```

The default gate rejects a dirty working tree; use `-AllowDirty` only for a deliberate local build. There is no scheduled or automated release path.
