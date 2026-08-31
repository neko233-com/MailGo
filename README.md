# MailGo

MailGo is a Windows-first local-first email workspace for multiple Google, QQ, Outlook, and custom IMAP/SMTP accounts.

The current foundation includes:

- React + Vite desktop UI with three-pane unified inbox.
- `motion` transitions with `prefers-reduced-motion` support.
- Tree-shakeable `reicon-react` icon components.
- Multiple accounts, including multiple QQ accounts, account switching, and import/export with redacted credentials.
- Windows desktop also supports password-protected account migration: encrypted export/import keeps provider credentials inside an Argon2id + ChaCha20-Poly1305 bundle and writes them back only to Windows Credential Manager.
- Local-first UI state with offline cache indicators and a Rust IPC boundary for durable state.
- Provider quick links and guided authorization-code onboarding.
- Safe HTML preview mode, attachments, smart categories (including Apple Connect and Apple advertising), search, unread filter, star, reply, compose, theme switching, and user CSS overrides.
- Local-only advertising classification can optionally hide ads from normal lists while keeping Apple Connect security mail and smart-category access visible.
- Rust `neko233-com/rdesktop` WebView2 shell with custom frameless title bar and preserved WebView data directory under `%LOCALAPPDATA%\\MailGo\\WebView2`.
- Windows Credential Manager integration through `keyring` for authorization-code storage; secrets never enter `state.json` or account exports.
- Native IMAP sync uses capability-driven `QRESYNC`/`CONDSTORE` deltas when a server exposes them (including `HIGHESTMODSEQ` cursors, `VANISHED` deletions, and UID-only new-mail discovery), with a UID-based incremental header fallback across provider folder mappings. Bounded `UID FLAGS` refreshes, lazy full-message retrieval, replayable offline flag mutations, local flag updates, and protected mailbox/attachment caches keep the offline view safe: Windows uses DPAPI, while non-Windows builds use an OS keyring-held XChaCha20-Poly1305 key. Attachment downloads use bounded start/chunk/cancel IPC with progress and cancellation support.
- Native sync discovers selectable IMAP mailboxes (including custom folders), persists a bounded per-account folder index, and exposes those folders in the desktop sidebar for offline browsing and UID pagination.
- Search combines the local cache's immediate results with a debounced, bounded native IMAP search across all discovered folders and accounts; server hits are merged into the encrypted cache with their UIDVALIDITY context before they reach the renderer.
- Network operations fail predictably: IMAP resolves/connects through bounded socket attempts with TLS/STARTTLS and read/write deadlines, OAuth HTTP calls use bounded connect/read/write timeouts and preserve bounded numeric `Retry-After` guidance for HTTP 429 responses, and SMTP sends have a bounded transport timeout so background sync and the offline outbox cannot hang indefinitely.
- Mail actions use UID semantics end-to-end: batch selection supports mark-read, archive, and delete; archive/trash operations update encrypted local folder caches immediately and queue provider mutations when offline. Gmail archive removes the Inbox label instead of copying the message into a duplicate folder.
- The desktop cache footer reports the number of encrypted flag/move mutations still waiting for provider replay across all accounts; the renderer receives counts only, never queue contents.
- Native mode also has an encrypted, bounded offline 发件箱: transient SMTP/network failures are queued without credentials, retried with bounded backoff, paused after repeated permanent failures, and resumed after reauthorization.
- Native SMTP sending supports plain text and HTML alternatives through provider-specific TLS/STARTTLS defaults.
- Compose supports bounded To/CC/BCC recipient lists, safe HTML alternatives, and chunked multi-file attachments without putting attachment bytes in a single IPC request.
- Reply, reply-all, and forward preserve the active account, prefill recipients from MIME To/CC headers, add safe subject prefixes, and quote the original message without restoring an unrelated draft.
- The desktop keyboard flow includes `C` for compose, `R` for replying to the selected message, `Ctrl/Cmd+K` for search, and `Esc` for closing transient UI.
- Native mode automatically saves and restores the latest text draft per account in a DPAPI-protected local store; sending removes the draft, while attachments remain intentionally session-scoped.
- Mailbox caches and MIME payloads have explicit byte/count limits; cached mutations are bound to UIDVALIDITY, and cache/outbox writes are serialized to avoid scheduler/IPC races.
- Native mode also surfaces those encrypted local drafts in the 草稿箱 list, with per-account counts, draft-specific continue-editing actions, and an explicit discard action.
- Windows tray lifecycle is implemented with the generated `resources/icons/mailgo.ico`: close-to-tray, restore on click, deliberate quit, and a five-minute background sync scheduler.
- Custom IMAP/SMTP onboarding accepts host, port, TLS mode, and password/app-password/OAuth2 settings without putting credentials in metadata.

Provider authentication is deliberately explicit: Gmail defaults to native OAuth2 and also offers a provider-issued app-password fallback; QQ uses its provider-issued authorization code; Outlook uses OAuth2 Device Flow or loopback PKCE; custom OAuth2 accounts can use a provider-issued Bearer access token. Set `MAILGO_GOOGLE_CLIENT_ID` or `MAILGO_OUTLOOK_CLIENT_ID` (and an optional redirect URI/client secret) to enable the native OAuth2 flow. The app never persists the one-time code itself.

For a registered desktop OAuth client, configure the client before launching the native shell:

```powershell
$env:MAILGO_GOOGLE_CLIENT_ID = "your-registered-google-client-id"
$env:MAILGO_GOOGLE_REDIRECT_URI = "http://127.0.0.1:8765/oauth/callback"
$env:MAILGO_OUTLOOK_CLIENT_ID = "your-registered-microsoft-client-id"
$env:MAILGO_OUTLOOK_REDIRECT_URI = "http://127.0.0.1:8765/oauth/callback"
```

The redirect URI must be registered exactly with the provider. MailGo keeps a shared listener for each configured `127.0.0.1` callback port/path and routes simultaneous OAuth returns by their validated `state`; the account assistant exchanges the returned code directly, while manual code entry remains available when the callback port is unavailable. Outlook uses a native Device Flow path in the account assistant: it opens the verification page, displays the user code, polls with provider-supplied intervals, and keeps the resulting token only in Windows Credential Manager.

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

The per-user `MailGo-rdesktop-updater` task runs weekly. It installs the exact reviewed upstream revision pinned in `scripts/update-rdesktop.ps1`; if Windows application control blocks Cargo build scripts, it only accepts a newer official release when `MAILGO_RDESKTOP_SIGNER_THUMBPRINT` is configured and both the SHA-256 digest and Authenticode signer thumbprint match. Without that independent signer trust root, it preserves the current installation without downgrading it. Updating either trust root is an intentional dependency-review step.

## Build the Windows shell

```powershell
npm run native:build
npm run native:run
```

The native shell loads `dist/` through the framework-owned `rdesktop://` protocol. Run `npm run build` before compiling Rust.

Create a self-contained portable Windows package with the release shell and renderer assets:

```powershell
npm run package:windows
# artifacts\MailGo-0.1.0-windows-x64.zip
```

Extract the archive and launch `MailGo.exe`. The package expects a compatible WebView2 runtime on Windows. The installed rdesktop 0.1.8 CLI currently emits a small NSIS placeholder from `rdesktop bundle`; do not distribute that generated file as an installer. Generate the portable archive on a release build host where the Windows application-control policy permits Cargo Release build scripts; this repository intentionally does not ship an unsigned or placeholder installer.
The Windows npm scripts place Cargo's target directory under `%LOCALAPPDATA%\MailGo\cargo-target` so application-control policies do not block dependency build scripts in the repository checkout.

## Publishing

This repository has no GitHub Actions. Releases and publication are intentionally manual and require an explicit user instruction. See [AGENTS.md](AGENTS.md).
