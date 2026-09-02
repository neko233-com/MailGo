import { existsSync, readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const assert = (condition, message) => {
  if (!condition) throw new Error(message)
}

const app = read('src/App.tsx')
const nativeMain = read('native/src/main.rs')
const oauth = read('native/src/oauth.rs')
const tray = read('native/src/tray.rs')
const cargo = read('native/Cargo.toml')
const installer = read('scripts/install-portable.ps1')
const release = read('scripts/release-windows.ps1')

assert(!app.includes('accounts.export_encrypted'), 'renderer must not expose credential-bearing export')
assert(!app.includes('accounts.import_encrypted'), 'renderer must not expose credential-bearing import')
assert(!nativeMain.includes('accounts.export_encrypted'), 'native IPC must not expose credential-bearing export')
assert(!nativeMain.includes('accounts.import_encrypted'), 'native IPC must not expose credential-bearing import')
assert(!nativeMain.includes('            "mail.attachment" =>'), 'attachments must not expose the legacy one-shot IPC path')
assert(!existsSync(resolve(root, 'native/src/transfer.rs')), 'credential transfer implementation must stay removed')
assert(!cargo.includes('argon2 ='), 'credential-transfer-only KDF dependency must stay removed')

assert(nativeMain.includes('CREDENTIAL_ENVELOPE_PREFIX'), 'stored credentials must use a versioned envelope')
assert(nativeMain.includes('credential_binding(account)'), 'stored credentials must be bound to account connection metadata')
assert(nativeMain.includes('account connection settings changed; reauthorization required'), 'binding mismatch must fail closed')
assert(nativeMain.includes('legacy custom account credentials require reauthorization before connecting'), 'unbound custom credentials must never connect before reauthorization')

assert(oauth.includes('CALLBACK_REQUEST_DEADLINE'), 'OAuth callbacks need an absolute request deadline')
assert(oauth.includes('checked_duration_since(Instant::now())'), 'OAuth read timeout must shrink with the remaining deadline')

assert(!tray.includes('FindWindowW'), 'single-instance activation must not trust a window title alone')
assert(tray.includes('QueryFullProcessImageNameW'), 'single-instance activation must verify the owning executable')
assert(tray.includes('find_main_window_for_process(GetCurrentProcessId())'), 'tray subclassing must stay within the current process')

assert(installer.includes('AllowUnsignedDevelopmentBuild'), 'unsigned installation must be explicitly development-only')
assert(installer.includes('portable ZIP installation is restricted to local source-build verification'), 'portable installation must fail closed outside explicit development use')
assert(installer.includes('verified signed MSIX for production'), 'production installation must route to whole-package signing')
assert(!installer.includes('TrustedSignerThumbprint'), 'an executable-only signature must not imply that portable renderer assets are authenticated')
assert(!release.includes('gh release create'), 'local release gate must not publish portable artifacts')
assert(!release.includes('[switch]$Publish'), 'portable release gate must not expose a publish switch')

assert(app.includes("label: 'Apple Connect 通知', icon: 'bell'"), 'Apple heuristic category must use neutral presentation')
assert(!app.includes("label: 'Apple Connect', icon: 'shieldCheck'"), 'heuristic From-domain classification must not imply verification')

console.log('security policy regression checks passed')
