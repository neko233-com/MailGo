import assert from 'node:assert/strict'
import { gzipSync } from 'node:zlib'
import { readFileSync, readdirSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const app = read('src/App.tsx')
const accountModal = read('src/components/AccountModal.tsx')
const authorizationPanel = read('src/components/AuthorizationPanel.tsx')
const compose = read('src/components/ComposeModal.tsx')
const helpModal = read('src/components/HelpModal.tsx')
const settingsPopover = read('src/components/SettingsPopover.tsx')
const styles = read('src/styles.css')
const html = read('dist/index.html')
const assetsRoot = resolve(root, 'dist/assets')
const assets = readdirSync(assetsRoot)

assert.doesNotMatch(app, /import \{[^}]*sample(?:Accounts|Mails|OutboxItems)[^}]*\} from '\.\/data'/)
assert.match(app, /import\('\.\/demoData'\)/)

for (const component of [
  'ConfirmDialog',
  'ExternalLinkDialog',
  'MailRuleManager',
  'OutboxDetail',
  'ComposeModal',
  'AccountModal',
  'AuthorizationPanel',
  'HelpModal',
  'SettingsPopover',
]) {
  assert.match(app, new RegExp(`lazy\\(async \\(\\) => \\({ default: \\(await import\\('\\.\\/components\\/${component}'\\)\\)\\.${component} }\\)\\)`))
  assert.ok(assets.some((asset) => asset.startsWith(`${component}-`) && asset.endsWith('.js')), `missing deferred ${component} chunk`)
}

for (const component of ['RecipientInput', 'RichTextEditor', 'ScheduleSendControl']) {
  assert.match(compose, new RegExp(`import \\{ ${component} \\} from '\\.\\/${component}'`))
}

assert.match(app, /<Suspense fallback=\{<DeferredModalLoading label="正在打开写信窗口…" \/>\}>/)
assert.match(app, /<Suspense fallback=\{<DeferredModalLoading label="正在打开账户设置…" \/>\}>/)
assert.match(app, /<Suspense fallback=\{<DeferredModalLoading label="正在载入帮助中心…" \/>\}>/)
assert.match(app, /<Suspense fallback=\{<DeferredAuthorizationPanelLoading isMobileOpen=\{isMobileAuthOpen\} \/>\}><AuthorizationPanel/)
assert.match(app, /<Suspense fallback=\{<DeferredPaneLoading label="正在载入发件箱详情…" \/>\}>/)
assert.match(app, /<Suspense fallback=\{<DeferredSettingsPopoverLoading \/>\}><SettingsPopover/)
assert.match(styles, /\.deferred-modal-loading/)
assert.match(styles, /\.deferred-pane-loading/)
assert.match(styles, /\.deferred-settings-popover-loading/)
assert.match(styles, /\.deferred-auth-panel-loading/)
assert.doesNotMatch(app, /function (?:AccountModal|AuthorizationPanel|HelpModal|ConnectionDiagnosticCard)/)
assert.doesNotMatch(app, /className="auth-card"/)
assert.doesNotMatch(app, /className="settings-title"/)
assert.match(accountModal, /export function AccountModal/)
assert.match(authorizationPanel, /export function AuthorizationPanel/)
assert.match(helpModal, /export function HelpModal/)
assert.match(settingsPopover, /export function SettingsPopover/)
assert.match(settingsPopover, /import \{ AccountSignatureSettings \} from '\.\/AccountSignatureSettings'/)
assert.doesNotMatch(app, /import\('\.\/components\/AccountSignatureSettings'\)/)
assert.equal(assets.some((asset) => asset.startsWith('AccountSignatureSettings-')), false, 'settings signatures must not create a nested deferred chunk waterfall')
assert.match(app, /import \{ TooltipButton \} from '\.\/components\/TooltipButton'/)
assert.match(compose, /import \{ TooltipButton \} from '\.\/TooltipButton'/)
assert.doesNotMatch(compose, /function TooltipButton/)

const entryMatch = html.match(/<script type="module"[^>]+src="\.\/assets\/(index-[^"]+\.js)"/)
assert.ok(entryMatch, 'production HTML must reference the hashed entry bundle')
const entry = readFileSync(resolve(assetsRoot, entryMatch[1]))
assert.ok(entry.length <= 330_000, `entry bundle is too large: ${entry.length} bytes`)
assert.ok(gzipSync(entry).length <= 106_000, `gzipped entry bundle is too large: ${gzipSync(entry).length} bytes`)
assert.equal(entry.includes(Buffer.from('Q3 launch plan')), false, 'browser demo messages must not enter the native startup bundle')

console.log(`Startup entry is ${entry.length} bytes (${gzipSync(entry).length} gzip) with localized deferred UI chunks.`)
