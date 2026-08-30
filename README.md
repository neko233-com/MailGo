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

## Run the browser development surface

```powershell
npm install
npm run dev
```

For rdesktop Agent-first development, run the rdesktop CLI from the sibling checkout:

```powershell
cargo run --manifest-path ..\\rdesktop\\Cargo.toml -p rdesktop-cli -- dev --path .
```

## Build the Windows shell

```powershell
npm run native:build
npm run native:run
```

The native shell loads `dist/` through the framework-owned `rdesktop://` protocol. Run `npm run build` before compiling Rust.

## Publishing

This repository has no GitHub Actions. Releases and publication are intentionally manual and require an explicit user instruction. See [AGENTS.md](AGENTS.md).
