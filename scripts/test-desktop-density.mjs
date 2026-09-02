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

assert(compactRowHeight === 40, 'compact virtual mail rows must remain desktop-dense')
assert(compactGroupHeight === 18, 'compact virtual date groups must remain desktop-dense')
assert(mobileRowHeight === 58, 'mobile virtual mail rows must match the touch-friendly mobile CSS height')
assert(app.includes('[isCompactDensity, isMobileLayout, mailListVirtualizer]'), 'virtual rows must be remeasured when the layout crosses the mobile breakpoint')
assert(cssRowHeight === compactRowHeight, 'virtual mail row height must match its compact CSS height')
assert(styles.includes('.app-shell.is-compact-density .titlebar { height: 38px;'), 'compact title bar must stay within the desktop height budget')
assert(styles.includes('.app-shell.is-compact-density .workspace { height: calc(100% - 38px);'), 'workspace height must match the compact title bar')
assert(styles.includes('.workspace.is-sidebar-collapsed { grid-template-columns: 52px minmax(304px, 332px) minmax(0, 1fr);'), '1366px desktop layout must reserve most space for the reading pane')
assert(styles.includes('.app-shell.is-compact-density .compose-body { min-height: 178px;'), 'compose must fit comfortably inside a short desktop window')
assert(styles.includes('.titlebar, .app-shell.is-compact-density .titlebar { height: 44px;'), 'narrow-window title bar must override compact desktop specificity')
assert(styles.includes('.workspace, .app-shell.is-compact-density .workspace { position: relative; height: calc(100% - 44px);'), 'narrow workspace height must match its title bar')
assert(styles.includes('.html-rendered :where(font, big, small) { font-size: inherit !important;'), 'legacy HTML font sizing must not override the reading pane')
assert(styles.includes('.html-rendered :where(table) { width: 100% !important;'), 'HTML email tables must remain inside the reading pane')
assert(app.includes("'border', 'cellpadding', 'cellspacing', 'face', 'height', 'nowrap', 'size', 'width'"), 'renderer sanitizer must remove presentational sizing attributes')
assert(nativeMain.includes('width: 1180,') && nativeMain.includes('height: 720,'), 'native window must open at a compact desktop size')
assert(nativeMain.includes('min_size: Some((920, 600))'), 'native window must retain a usable resizable desktop minimum')
assert(rdesktopConfig.includes('width = 1180') && rdesktopConfig.includes('height = 720'), 'rdesktop configuration must match the native window size')

console.log('desktop density regression checks passed')
