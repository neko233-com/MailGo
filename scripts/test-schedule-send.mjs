import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import {
  MAX_SCHEDULE_AHEAD_MS,
  MIN_SCHEDULE_LEAD_MS,
  defaultCustomSchedule,
  formatScheduledAt,
  getScheduleSuggestions,
  toLocalDateTimeInputValue,
  validateScheduledAt,
} from '../src/scheduleSend.ts'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const app = read('src/App.tsx')
const component = read('src/components/ScheduleSendControl.tsx')
const detail = read('src/components/OutboxDetail.tsx')
const data = read('src/data.ts')
const types = read('src/types.ts')
const styles = read('src/styles.css')
const main = read('native/src/main.rs')
const outbox = read('native/src/outbox.rs')
const sync = read('native/src/sync.rs')
const release = read('scripts/release-windows.ps1')

const fridayEvening = new Date(2026, 0, 30, 19, 0, 0, 0)
const suggestions = getScheduleSuggestions(fridayEvening)
assert.equal(suggestions[0].label, '今晚 20:00')
assert.equal(suggestions[0].timestamp, new Date(2026, 0, 30, 20, 0, 0, 0).getTime())
assert.equal(suggestions[1].timestamp, new Date(2026, 0, 31, 8, 0, 0, 0).getTime())
assert.equal(suggestions[2].timestamp, new Date(2026, 1, 2, 8, 0, 0, 0).getTime())

const monthBoundary = getScheduleSuggestions(new Date(2026, 0, 31, 21, 0, 0, 0))
assert.equal(monthBoundary[0].label, '明晚 20:00')
assert.equal(monthBoundary[0].timestamp, new Date(2026, 1, 1, 20, 0, 0, 0).getTime())

const now = new Date(2026, 5, 18, 10, 10, 0, 0).getTime()
assert.equal(validateScheduledAt(now + MIN_SCHEDULE_LEAD_MS, now).ok, true)
assert.equal(validateScheduledAt(now + MIN_SCHEDULE_LEAD_MS - 1, now).ok, false)
assert.equal(validateScheduledAt(now + MAX_SCHEDULE_AHEAD_MS, now).ok, true)
assert.equal(validateScheduledAt(now + MAX_SCHEDULE_AHEAD_MS + 1, now).ok, false)
assert.equal(validateScheduledAt(Number.NaN, now).ok, false)
const custom = defaultCustomSchedule(now)
assert(custom >= now + 30 * 60_000)
assert.equal(new Date(custom).getMinutes() % 30, 0)
assert.equal(new Date(toLocalDateTimeInputValue(custom)).getTime(), custom)
assert.equal(formatScheduledAt(Number.NaN), '无效时间')

assert.match(component, /className="compose-send-split"/)
assert.match(component, /type="datetime-local"/)
assert.match(component, /<form noValidate onSubmit=\{submitCustom\}>/)
assert.match(component, /validateScheduledAt/)
assert.match(component, /邮件会加密保存在本机发件箱/)
assert.match(app, /<ScheduleSendControl/)
assert.match(app, /scheduledFor \? \{ scheduledFor \} : \{\}/)
assert.match(app, /result\.scheduled && result\.scheduledFor/)
assert.match(app, /snapshot\.status\.userScheduled/)
assert.match(app, /snapshot\.status\.undoable/)
assert.match(app, /\.rich-compose-link, \.compose-schedule-menu/)
assert.match(detail, /label: '定时发送'/)
assert.match(detail, /hasUserSchedule \? '立即发送'/)
assert.match(data, /scheduledAt: sampleOutboxNow \+ 7_200/)
assert.match(types, /scheduledFor\?: number/)
assert.match(types, /scheduledAt\?: number/)
assert.match(styles, /\.compose-schedule-menu/)
assert.match(styles, /\.compose-modal \{[^}]*overflow: visible;/)

assert.match(main, /optional_u64_field\(&message\.payload, "scheduledFor"\)/)
assert.match(main, /outbox::enqueue_at/)
assert.match(main, /"scheduledFor": queued\.scheduled_at/)
assert.match(outbox, /pub const MIN_SCHEDULE_LEAD_SECONDS: u64 = 60/)
assert.match(outbox, /pub const MAX_SCHEDULE_AHEAD_SECONDS: u64 = 366 \* 24 \* 60 \* 60/)
assert.match(outbox, /pub scheduled_at: Option<u64>/)
assert.match(outbox, /pub user_scheduled: usize/)
assert.match(outbox, /pub undoable: usize/)
assert.match(outbox, /message\.scheduled_at\.filter\(\|at\| \*at > now\)\.unwrap_or\(now\)/)
assert.match(sync, /wait_for_scheduler_change/)
assert.doesNotMatch(sync, /mailgo-outbox-[^"\n]*\{/)
assert.match(release, /npm run test:schedule-send/)

console.log('Encrypted scheduled-send timing, queue distinction, UI controls, and release-gate checks passed.')
