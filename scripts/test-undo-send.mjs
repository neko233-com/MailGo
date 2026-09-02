import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), 'utf8')
const main = read('native/src/main.rs')
const outbox = read('native/src/outbox.rs')
const sync = read('native/src/sync.rs')
const send = read('native/src/send.rs')
const app = read('src/App.tsx')
const styles = read('src/styles.css')

assert.match(main, /const DEFAULT_UNDO_SEND_SECONDS: u64 = 10/)
assert.match(main, /"app\.set_undo_send_seconds" =>/)
assert.match(main, /"mail\.outbox\.undo" =>/)
assert.match(main, /send::validate_message\(&validation_message, &attachments\)\?/)
assert.match(main, /outbox::enqueue_with_delay/)
assert.match(main, /"undoExpiresAt"/)

assert.match(outbox, /message\.attempts > 0 \|\| message\.next_attempt_at <= now/)
assert.match(outbox, /pub fn wait_for_scheduler_change/)
assert.match(outbox, /Condvar/)
assert.match(outbox, /pub draft_id: Option<String>/)
assert.match(send, /pub fn validate_message/)

assert.match(sync, /name\("mailgo-outbox-scheduler"\.into\(\)\)/)
assert.match(sync, /next_due_delay/)
assert.match(sync, /wait_for_scheduler_change/)
assert.doesNotMatch(sync, /mailgo-outbox-[^"\n]*\{/) // one scheduler, never one thread per message

assert.match(app, /撤销发送/)
assert.match(app, /mail\.outbox\.undo/)
assert.match(app, /!sendResult\.queued/)
assert.match(app, /openCompose\(action\.draftId\)/)
assert.match(app, /currentDraftId \? \{ draftId: currentDraftId \}/)
assert.match(styles, /animation: toast-progress var\(--toast-duration\) linear forwards/)

console.log('Undo-send scheduling, cancellation, draft recovery, and UI timing checks passed.')
