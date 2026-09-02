import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const assert = (condition, message) => {
  if (!condition) throw new Error(message)
}

const app = read('src/App.tsx')
const styles = read('src/styles.css')
const nativeMain = read('native/src/main.rs')
const rdesktopConfig = read('rdesktop.toml')

const compactRowHeight = Number(app.match(/const COMPACT_MAIL_ROW_HEIGHT = (\d+)/)?.[1])
const compactGroupHeight = Number(app.match(/const COMPACT_MAIL_GROUP_HEIGHT = (\d+)/)?.[1])
const mobileRowHeight = Number(app.match(/const MOBILE_MAIL_ROW_HEIGHT = (\d+)/)?.[1])
const cssRowHeight = Number(styles.match(/\.app-shell\.is-compact-density \.mail-row \{ min-height: (\d+)px/)?.[1])

assert(compactRowHeight === 36, 'compact virtual mail rows must remain desktop-dense')
assert(compactGroupHeight === 16, 'compact virtual date groups must remain desktop-dense')
assert(mobileRowHeight === 44, 'narrow desktop virtual mail rows must remain compact')
assert(app.includes('[isCompactDensity, isMobileLayout, mailListVirtualizer]'), 'virtual rows must be remeasured when the layout crosses the mobile breakpoint')
assert(cssRowHeight === compactRowHeight, 'virtual mail row height must match its compact CSS height')
assert(styles.includes('.app-shell.is-compact-density .titlebar { height: 34px;'), 'compact title bar must stay within the desktop height budget')
assert(styles.includes('.app-shell.is-compact-density .workspace { height: calc(100% - 34px);'), 'workspace height must match the compact title bar')
assert(styles.includes('.app-shell.is-compact-density .workspace.is-sidebar-collapsed { grid-template-columns: 48px minmax(300px, 340px) minmax(0, 1fr);'), 'compact desktop layout must prioritize the reading pane')
assert(styles.includes('.workspace.is-sidebar-collapsed { grid-template-columns: 52px minmax(304px, 332px) minmax(0, 1fr);'), '1366px desktop layout must reserve most space for the reading pane')
assert(styles.includes('.app-shell.is-compact-density .compose-body { min-height: 150px;'), 'compose must fit comfortably inside a short desktop window')
assert(styles.includes('.titlebar, .app-shell.is-compact-density .titlebar { height: 38px;'), 'narrow-window title bar must remain desktop-dense')
assert(styles.includes('.workspace, .app-shell.is-compact-density .workspace { position: relative; height: calc(100% - 38px);'), 'narrow workspace height must match its title bar')
assert(styles.includes('.mobile-overlay { position: fixed; inset: 38px 0 0;'), 'narrow navigation overlay must start below the dense title bar')
assert(styles.includes('.html-rendered :where(*) { max-width: 100%; font-size: inherit !important;'), 'HTML descendants must not escape the reading-pane typography budget')
assert(styles.includes('.html-rendered :where(table) { width: auto !important; max-width: 100% !important;'), 'nested HTML email tables must fit without every table expanding to the pane width')
assert(app.includes("'border', 'cellpadding', 'cellspacing', 'face', 'height', 'nowrap', 'size', 'width'"), 'renderer sanitizer must remove presentational sizing attributes')
assert(app.includes('const DEFAULT_MAIL_CONTENT_SCALE: MailContentScale = 90'), 'mail content must default below the oversized 100% renderer baseline')
assert(app.includes('aria-label="邮件正文显示比例"'), 'mail readers must expose independent content scaling controls')
assert(app.includes("readLocalStorageValue('mailgo-mail-content-scale')"), 'mail content scaling must survive restarts')
assert(app.includes("readLocalStorageValue('mailgo-display-density-v2')"), 'the denser desktop default must supersede stale comfortable-mode preferences')
assert(nativeMain.includes('width: 1180,') && nativeMain.includes('height: 720,'), 'native window must open at a compact desktop size')
assert(nativeMain.includes('min_size: Some((920, 600))'), 'native window must retain a usable resizable desktop minimum')
assert(rdesktopConfig.includes('width = 1180') && rdesktopConfig.includes('height = 720'), 'rdesktop configuration must match the native window size')

console.log('desktop density regression checks passed')
