# MailGo

MailGo is a Windows-first local-first email workspace for multiple Google, QQ, Outlook, and custom IMAP/SMTP accounts.

The current foundation includes:

- React + Vite desktop UI with three-pane unified inbox.
- `motion` transitions with `prefers-reduced-motion` support.
- Tree-shakeable `reicon-react` icon components.
- Multiple accounts, including multiple QQ accounts, account switching, and import/export with redacted credentials.
- Local-first UI state with offline cache indicators and a Rust IPC boundary for durable state.
- Provider quick links and guided authorization-code onboarding.
- Safe HTML preview mode, attachments, smart categories (including Apple Connect and Apple advertising), search, unread filter, star, reply, compose, theme switching, and user CSS overrides.
- Rust `neko233-com/rdesktop` WebView2 shell with custom frameless title bar and preserved WebView data directory under `%LOCALAPPDATA%\\MailGo\\WebView2`.
- Windows Credential Manager integration through `keyring` for authorization-code storage; secrets never enter `state.json` or account exports.
- Native IMAP sync uses UID-based header caching across provider folder mappings, lazy full-message retrieval, replayable offline flag mutations, local flag updates, and DPAPI-protected mailbox/attachment caches on Windows. Attachment downloads use bounded start/chunk/cancel IPC with progress and cancellation support.
- Native SMTP sending supports plain text and HTML alternatives through provider-specific TLS/STARTTLS defaults.
- Windows tray lifecycle is implemented with the generated `resources/icons/mailgo.ico`: close-to-tray, restore on click, deliberate quit, and a five-minute background sync scheduler.
- Custom IMAP/SMTP onboarding accepts host, port, TLS mode, and password/app-password/OAuth2 settings without putting credentials in metadata.

Provider authentication is deliberately explicit: Gmail and QQ use provider-issued app passwords in the quick-start flow; Outlook can use OAuth2 Device Flow or loopback PKCE, and custom OAuth2 accounts can use a provider-issued Bearer access token. Set `MAILGO_GOOGLE_CLIENT_ID` or `MAILGO_OUTLOOK_CLIENT_ID` (and an optional redirect URI/client secret) to enable the native OAuth2 flow. The app never persists the one-time code itself.

For a registered desktop OAuth client, configure the client before launching the native shell:

```powershell
$env:MAILGO_GOOGLE_CLIENT_ID = "your-registered-google-client-id"
$env:MAILGO_GOOGLE_REDIRECT_URI = "http://127.0.0.1:8765/oauth/callback"
$env:MAILGO_OUTLOOK_CLIENT_ID = "your-registered-microsoft-client-id"
$env:MAILGO_OUTLOOK_REDIRECT_URI = "http://127.0.0.1:8765/oauth/callback"
```

The redirect URI must be registered exactly with the provider. MailGo listens once on a configured `127.0.0.1` callback and lets the account assistant exchange the returned code directly; manual code entry remains available when the callback port is unavailable. Outlook uses a native Device Flow path in the account assistant: it opens the verification page, displays the user code, polls with provider-supplied intervals, and keeps the resulting token only in Windows Credential Manager.

## Run the browser development surface

```powershell
npm install
npm run dev
```

For rdesktop Agent-first development, use the installed CLI (kept current by the local updater):

```powershell
rdesktop dev --path .
```

## Build the Windows shell

```powershell
npm run native:build
npm run native:run
```

The native shell loads `dist/` through the framework-owned `rdesktop://` protocol. Run `npm run build` before compiling Rust.

## Publishing

This repository has no GitHub Actions. Releases and publication are intentionally manual and require an explicit user instruction. See [AGENTS.md](AGENTS.md).
