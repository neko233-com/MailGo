import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import ts from 'typescript'

const root = resolve(import.meta.dirname, '..')
const read = (path) => readFileSync(resolve(root, path), 'utf8')
const source = read('src/snooze.ts')
const output = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2022, target: ts.ScriptTarget.ES2022 },
}).outputText
const snooze = await import(`data:text/javascript;base64,${Buffer.from(output).toString('base64')}`)

const thursdayMorning = new Date(2026, 8, 3, 10, 5, 0, 0)
const suggestions = snooze.snoozeSuggestions(thursdayMorning)
assert.deepEqual(suggestions.map((item) => item.id), ['later', 'tomorrow', 'weekend', 'next-week'])
assert.equal(new Date(suggestions[0].timestamp).getHours(), 18)
assert.equal(new Date(suggestions[1].timestamp).getDate(), 4)
assert.equal(new Date(suggestions[1].timestamp).getHours(), 8)
assert.equal(new Date(suggestions[2].timestamp).getDay(), 6)
assert.equal(new Date(suggestions[3].timestamp).getDay(), 1)
assert.ok(suggestions.every((item) => item.timestamp >= thursdayMorning.getTime() + snooze.MIN_SNOOZE_LEAD_MS))

const monthBoundary = snooze.snoozeSuggestions(new Date(2026, 7, 31, 23, 30, 0, 0))[0]
assert.equal(monthBoundary.label, '明天傍晚', 'same day-of-month in another month must not be labelled today')
const saturdayLate = snooze.snoozeSuggestions(new Date(2026, 8, 5, 12, 0, 0, 0))
assert.equal(new Date(saturdayLate.find((item) => item.id === 'weekend').timestamp).getDate(), 12)

const custom = snooze.defaultCustomSnoozeTime(new Date(2026, 8, 3, 10, 7, 45, 0))
assert.equal(custom.getHours(), 12)
assert.equal(custom.getMinutes(), 15)
assert.match(snooze.toLocalDateTimeInput(custom), /^2026-09-03T12:15$/)
assert.equal(snooze.validateSnoozeTime(1_059_999, 1_000_000), '提醒时间至少需要在 1 分钟后')
assert.equal(snooze.validateSnoozeTime(1_060_000, 1_000_000), '')
assert.equal(snooze.validateSnoozeTime(1_000_000 + snooze.MAX_SNOOZE_AHEAD_MS + 1, 1_000_000), '提醒时间不能超过 1 年')
assert.equal(snooze.formatSnoozeTime(Number.NaN), '无效时间')

const nativeStore = read('native/src/snooze.rs')
const nativeMain = read('native/src/main.rs')
const nativeSync = read('native/src/sync.rs')
const app = read('src/App.tsx')
const styles = read('src/styles.css')
const types = read('src/types.ts')
const release = read('scripts/release-windows.ps1')

assert.match(nativeMain, /mod snooze;/)
for (const command of ['mail.snooze.snapshot', 'mail.snooze', 'mail.unsnooze']) {
  assert.ok(nativeMain.includes(`"${command}"`), `missing native IPC command ${command}`)
}
assert.match(nativeMain, /sync::load_cached_message[\s\S]*?snooze::schedule/)
assert.match(nativeMain, /snooze::remove_account/)
assert.match(nativeStore, /protect_cache\(&payload\)/)
assert.match(nativeStore, /message\.text_body\.clear\(\)/)
assert.match(nativeStore, /message\.attachments\.clear\(\)/)
assert.match(nativeStore, /MAX_ITEMS: usize = 1_000/)
assert.match(nativeStore, /MIN_SNOOZE_LEAD_SECONDS: u64 = 60/)
assert.match(nativeStore, /MAX_SNOOZE_AHEAD_SECONDS: u64 = 366 \* 24 \* 60 \* 60/)
assert.match(nativeStore, /fn missing_primary_recovers_from_backup/)
assert.match(nativeSync, /spawn_snooze_scheduler/)
assert.match(nativeSync, /mailgo-snooze-scheduler/)
assert.match(nativeSync, /snoozed messages returned to inbox/)
assert.match(types, /'snoozed'/)
assert.match(types, /export interface NativeSnoozeSnapshot/)
assert.match(app, /<SnoozeControl/)
assert.match(app, /selectedFolder === 'snoozed'/)
assert.match(app, /setSnoozedMails/)
assert.match(app, /hydrated && !mailNeedsBodyHydration\(hydrated\)/)
assert.match(app, /const preferred = existing && !mailNeedsBodyHydration\(existing\) \? existing : mail/)
assert.match(app, /setSnoozedMails\(\(current\) => current\.map\(\(mail\) => mail\.id === candidate\.id/)
assert.match(app, /mail\.snoozedUntil != null\) continue/)
assert.match(app, /displayedAccountUnreadCounts/)
assert.match(app, /account\.unread - \(snoozedUnread\.get\(account\.id\) \?\? 0\)/)
assert.match(app, /SNOOZE_TIMER_RECHECK_MS = 5 \* 60 \* 1_000/)
assert.match(app, /window\.setTimeout\(scheduleCheck, SNOOZE_TIMER_RECHECK_MS\)/)
assert.match(app, /selectedMail\.id === 'empty-mail' \? <div className="reading-empty-state">/)
assert.match(styles, /\.reading-empty-state \{[^}]*place-content: center/)
assert.match(release, /npm run test:snooze/)

console.log('Encrypted snooze timing, native scheduler, local folder, and UI checks passed.')
