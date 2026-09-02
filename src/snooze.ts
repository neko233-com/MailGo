export const MIN_SNOOZE_LEAD_MS = 60_000
export const MAX_SNOOZE_AHEAD_MS = 366 * 24 * 60 * 60 * 1_000

export type SnoozeSuggestion = {
  id: 'later' | 'tomorrow' | 'weekend' | 'next-week'
  label: string
  detail: string
  timestamp: number
}

function atLocalTime(base: Date, dayOffset: number, hour: number) {
  const result = new Date(base)
  result.setDate(result.getDate() + dayOffset)
  result.setHours(hour, 0, 0, 0)
  return result
}

function isSameLocalDay(left: Date, right: Date) {
  return left.getFullYear() === right.getFullYear()
    && left.getMonth() === right.getMonth()
    && left.getDate() === right.getDate()
}

function nextWeekdayAt(base: Date, weekday: number, hour: number, requireFutureWeek = false) {
  let dayOffset = (weekday - base.getDay() + 7) % 7
  if (requireFutureWeek && dayOffset === 0) dayOffset = 7
  let result = atLocalTime(base, dayOffset, hour)
  if (result.getTime() < base.getTime() + MIN_SNOOZE_LEAD_MS) {
    result = atLocalTime(base, dayOffset + 7, hour)
  }
  return result
}

export function snoozeSuggestions(now = new Date()): SnoozeSuggestion[] {
  const laterToday = atLocalTime(now, 0, 18)
  const later = laterToday.getTime() >= now.getTime() + MIN_SNOOZE_LEAD_MS
    ? laterToday
    : atLocalTime(now, 1, 18)
  const tomorrow = atLocalTime(now, 1, 8)
  const weekend = nextWeekdayAt(now, 6, 9)
  const nextWeek = nextWeekdayAt(now, 1, 8, true)
  return [
    { id: 'later', label: isSameLocalDay(later, now) ? '今天晚些时候' : '明天傍晚', detail: formatSnoozeTime(later.getTime()), timestamp: later.getTime() },
    { id: 'tomorrow', label: '明天上午', detail: formatSnoozeTime(tomorrow.getTime()), timestamp: tomorrow.getTime() },
    { id: 'weekend', label: '本周末', detail: formatSnoozeTime(weekend.getTime()), timestamp: weekend.getTime() },
    { id: 'next-week', label: '下周一', detail: formatSnoozeTime(nextWeek.getTime()), timestamp: nextWeek.getTime() },
  ]
}

export function defaultCustomSnoozeTime(now = new Date()) {
  const result = new Date(now.getTime() + 2 * 60 * 60 * 1_000)
  result.setMinutes(Math.ceil(result.getMinutes() / 15) * 15, 0, 0)
  return result
}

export function toLocalDateTimeInput(value: Date) {
  if (Number.isNaN(value.getTime())) return ''
  const pad = (part: number) => String(part).padStart(2, '0')
  return `${value.getFullYear()}-${pad(value.getMonth() + 1)}-${pad(value.getDate())}T${pad(value.getHours())}:${pad(value.getMinutes())}`
}

export function validateSnoozeTime(timestamp: number, now = Date.now()) {
  if (!Number.isFinite(timestamp)) return '请选择有效的提醒时间'
  if (timestamp < now + MIN_SNOOZE_LEAD_MS) return '提醒时间至少需要在 1 分钟后'
  if (timestamp > now + MAX_SNOOZE_AHEAD_MS) return '提醒时间不能超过 1 年'
  return ''
}

export function formatSnoozeTime(timestamp: number) {
  const date = new Date(timestamp)
  if (Number.isNaN(date.getTime())) return '无效时间'
  const weekdays = ['周日', '周一', '周二', '周三', '周四', '周五', '周六']
  return `${date.getMonth() + 1}/${date.getDate()} ${weekdays[date.getDay()]} ${date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit', hour12: false })}`
}
