import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const app = read('src/App.tsx')
const data = read('src/data.ts')
const types = read('src/types.ts')
const outbox = read('native/src/outbox.rs')
const main = read('native/src/main.rs')
const sync = read('native/src/sync.rs')
const detail = read('src/components/OutboxDetail.tsx')
const confirmation = read('src/components/ConfirmDialog.tsx')

for (const command of ['snapshot', 'retry', 'recall', 'discard']) {
  assert.match(main, new RegExp(`"mail\\.outbox\\.${command}" =>`), `native IPC must expose ${command}`)
}

assert.match(outbox, /pub struct OutboxSnapshot/)
assert.match(outbox, /pub struct OutboxAttachmentSummary/)
assert.doesNotMatch(outbox.match(/pub struct OutboxAttachmentSummary[\s\S]*?\n}/)?.[0] ?? '', /bytes:/)
assert.match(outbox, /static IN_FLIGHT_IDS:/)
assert.match(outbox, /static FLUSHING_ACCOUNTS:/)
assert.doesNotMatch(outbox, /FLUSH_IN_PROGRESS/)
assert.match(outbox, /pub fn recall_to_draft/)
assert.match(outbox, /pub fn discard_queued/)
assert.match(outbox, /RetryOutboxStatus::TooLate/)
assert.match(outbox, /DiscardOutboxStatus::TooLate/)
assert.match(outbox, /RecallOutboxStatus::TooLate/)
assert.match(outbox, /message\.account_id != account_id \|\| !message\.paused/)
assert.match(sync, /map_with_concurrency\(&account_ids, ACCOUNT_SYNC_CONCURRENCY/)
assert.match(sync, /run_scheduled_outbox_account/)

assert.match(types, /'outbox'/)
assert.match(data, /label: '发件箱'/)
assert.match(app, /mail\.outbox\.snapshot/)
assert.match(app, /folder === 'outbox'\) \{[\s\S]*?refreshOutbox\(\)[\s\S]*?return/)
assert.match(app, /selectedFolder !== 'starred' && selectedFolder !== 'outbox'/)
assert.match(app, /queuedDraftKeys/)
assert.match(app, /!sendResult\.queued/)
assert.match(app, /<OutboxDetail/)
assert.match(app, /<ConfirmDialog/)
const discardHandler = app.match(/const discardQueuedMessage = async \(\) => \{[\s\S]*?\n  \}/)?.[0] ?? ''
assert.doesNotMatch(discardHandler, /window\.confirm\(/)
assert.match(detail, /为保证发件箱秒开/)
assert.match(confirmation, /role="alertdialog"/)
assert.match(confirmation, /cancelRef\.current\?\.focus\(\)/)

console.log('Local outbox snapshot, race protection, draft ownership, and UI management checks passed.')
