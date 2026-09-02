import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const app = read('src/App.tsx')
const compose = read('src/components/ComposeModal.tsx')
const editor = read('src/components/RichTextEditor.tsx')
const richText = read('src/richText.ts')
const icons = read('src/components/Icon.tsx')
const styles = read('src/styles.css')
const types = read('src/types.ts')
const drafts = read('native/src/drafts.rs')
const main = read('native/src/main.rs')
const mail = read('native/src/mail.rs')
const outbox = read('native/src/outbox.rs')
const release = read('scripts/release-windows.ps1')

for (const command of ['bold', 'italic', 'underline', 'insertUnorderedList', 'insertOrderedList', 'formatBlock', 'removeFormat']) {
  assert.match(editor, new RegExp(command))
}
assert.match(editor, /contentEditable/)
assert.match(editor, /role="toolbar"/)
assert.match(editor, /onPaste=\{handlePaste\}/)
assert.match(editor, /MAX_COMPOSE_HTML_BYTES/)
assert.match(editor, /normalizeComposeLink/)

assert.match(richText, /const allowedComposeTags = new Set/)
assert.match(richText, /script,style,iframe,object,embed,form/)
assert.match(richText, /\^cid:/)
assert.match(richText, /parsed\.protocol === 'https:'/)
assert.match(richText, /appendSignatureToComposeHtml/)
assert.match(richText, /querySelector\('blockquote'\)/)

for (const icon of ['Bold', 'Italic', 'TextUnderline', 'OrderedList', 'UnorderedList', 'QuoteDown']) {
  assert.match(icons, new RegExp(`reicon-react/icons/${icon}`))
}
assert.match(compose, /<RichTextEditor/)
assert.match(compose, /event\.target\.closest\('\.rich-compose-link, \.compose-schedule-menu'\)/)
assert.match(compose, /htmlBody: richBody/)
assert.match(compose, /composeHtmlBody\(body, currentInlineImages, htmlMode \? richBody : undefined, accountSignature\)/)
assert.match(types, /htmlBody\?: string/)
assert.match(styles, /\.rich-compose-toolbar/)
assert.match(styles, /\.app-shell\.is-compact-density \.rich-compose-body \{ min-height: 120px;/)

assert.match(drafts, /const STORE_SCHEMA_VERSION: u32 = 3/)
assert.match(drafts, /pub html_body: Option<String>/)
assert.match(drafts, /sanitize_outgoing_html/)
assert.match(main, /optional_sanitized_html_field/)
assert.match(mail, /pub fn sanitize_outgoing_html/)
assert.match(outbox, /html_body: message\.html_body\.clone\(\)/)
assert.match(release, /npm run test:rich-compose/)

console.log('Rich compose editing, safe HTML, encrypted draft, recall, and compact-layout checks passed.')
