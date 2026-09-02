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
const cssRowHeight = Number(styles.match(/\.app-shell\.is-compact-density \.mail-row \{ min-height: (\d+)px/)?.[1])

assert(compactRowHeight === 44, 'compact virtual mail rows must remain desktop-dense')
assert(compactGroupHeight === 20, 'compact virtual date groups must remain desktop-dense')
assert(cssRowHeight === compactRowHeight, 'virtual mail row height must match its compact CSS height')
assert(styles.includes('.app-shell.is-compact-density .titlebar { height: 42px;'), 'compact title bar must stay within the desktop height budget')
assert(styles.includes('.app-shell.is-compact-density .workspace { height: calc(100% - 42px);'), 'workspace height must match the compact title bar')
assert(styles.includes('.html-rendered :where(font, big, small) { font-size: inherit !important;'), 'legacy HTML font sizing must not override the reading pane')
assert(styles.includes('.html-rendered :where(table) { width: 100% !important;'), 'HTML email tables must remain inside the reading pane')
assert(app.includes("'border', 'cellpadding', 'cellspacing', 'face', 'height', 'nowrap', 'size', 'width'"), 'renderer sanitizer must remove presentational sizing attributes')
assert(nativeMain.includes('width: 1180,') && nativeMain.includes('height: 720,'), 'native window must open at a compact desktop size')
assert(nativeMain.includes('min_size: Some((920, 600))'), 'native window must retain a usable resizable desktop minimum')
assert(rdesktopConfig.includes('width = 1180') && rdesktopConfig.includes('height = 720'), 'rdesktop configuration must match the native window size')

console.log('desktop density regression checks passed')
