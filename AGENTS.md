# MailGo Agent Instructions

## Product scope

MailGo is a Windows-first, local-first multi-account email client built with React/Vite and the Rust `neko233-com/rdesktop` WebView2 shell. The UI uses `motion` for restrained state transitions and `reicon-react` for the icon system.

## Security and test data

- Any owner-provided test mailbox is development-only data; keep its address in a local-only copy of this file and never use it in screenshots, fixtures, telemetry, documentation examples, or release notes.
- Never write, print, commit, or paste any mailbox authorization code, password, OAuth refresh token, session cookie, or exported secret into source files, AGENTS.md, issues, logs, or git history.
- Runtime mailbox credentials belong in the OS credential store (Windows Credential Manager via the Rust `keyring` adapter). `state.json` is metadata only; the Windows mailbox cache is DPAPI-protected.
- Account import/export is deliberately redacted. An exported file requires re-authorization after import.
- If a local secret is needed for manual testing, keep it outside the repository under an ignored path and remove it after the test.

## Release and automation policy

- Do not create or enable GitHub Actions that auto-publish, auto-release, or auto-deploy. The repository intentionally has no `.github/workflows` directory.
- Publishing is a manual, user-gated action. Only publish when the user explicitly says `发布` (or clearly authorizes a specific release operation in the current turn).
- Local development may use a scheduled rdesktop toolchain updater, but it must never publish MailGo artifacts.

## Commands

- `npm install` — install frontend dependencies.
- `npm run build` — type-check and build the Vite frontend into `dist/`.
- `npm run native:build` — build frontend and compile the native rdesktop shell.
- `npm run native:run` — build frontend and launch the native shell.
- `cargo fmt --manifest-path native/Cargo.toml -- --check` — format check for native code.

## Architecture rules

- Keep the app shell, mail list, reader, account/auth flows, settings, and data adapters in focused modules; do not turn `App.tsx` into a backend.
- Keep provider-specific connection details behind a provider adapter boundary. Google, QQ, Outlook, and custom IMAP/SMTP must not leak provider quirks into presentation components.
- Sanitize remote HTML before rendering and keep remote images subject to user privacy settings. Never execute script, form, iframe, or `javascript:` content from a message.
- Preserve offline-first behavior: cached metadata and message bodies remain readable without a network; sync is resumable and must not block the UI thread.
- Attachment transfers use the chunked `mail.attachment.start/chunk/cancel` IPC contract; keep each WebView payload bounded and clean up native download sessions on completion, cancellation, or failure.
- Window close should hide to the real Windows tray when the preference is enabled; the tray restores the window and only a deliberate tray quit should terminate the background process. The native scheduler may continue IMAP refreshes while hidden.
- Google/Gmail quick-start uses an app password, QQ uses its provider authorization code/app password, and Outlook/custom OAuth2 can use a provider-issued Bearer token. When `MAILGO_GOOGLE_CLIENT_ID` or `MAILGO_OUTLOOK_CLIENT_ID` is provisioned, native OAuth2 + PKCE or Outlook Device Flow stores the resulting token bundle only in the OS credential store; authorization codes and device codes stay in memory and never enter metadata.
