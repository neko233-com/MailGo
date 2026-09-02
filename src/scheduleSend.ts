export const MIN_SCHEDULE_LEAD_MS = 60_000
export const MAX_SCHEDULE_AHEAD_MS = 366 * 24 * 60 * 60 * 1_000

export type ScheduleSuggestion = {
  id: 'later' | 'tomorrow' | 'next-week'
  label: string
  detail: string
  timestamp: number
}

export type ScheduleValidation =
  | { ok: true; timestamp: number }
  | { ok: false; error: string }

const dateFormatter = new Intl.DateTimeFormat('zh-CN', {
  month: 'numeric',
  day: 'numeric',
  weekday: 'short',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

const shortDateFormatter = new Intl.DateTimeFormat('zh-CN', {
  month: 'numeric',
  day: 'numeric',
  hour: '2-digit',
  minute: '2-digit',
  hour12: false,
})

function pad(value: number) {
  return String(value).padStart(2, '0')
}

function atLocalTime(source: Date, dayOffset: number, hour: number, minute = 0) {
  const date = new Date(source)
  date.setDate(date.getDate() + dayOffset)
  date.setHours(hour, minute, 0, 0)
  return date
}

function isSameLocalDay(left: Date, right: Date) {
  return left.getFullYear() === right.getFullYear()
    && left.getMonth() === right.getMonth()
    && left.getDate() === right.getDate()
}

export function toLocalDateTimeInputValue(timestamp: number) {
  const date = new Date(timestamp)
  if (Number.isNaN(date.getTime())) return ''
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

export function formatScheduledAt(timestamp: number) {
  if (!Number.isFinite(timestamp)) return '无效时间'
  const date = new Date(timestamp)
  return Number.isNaN(date.getTime()) ? '无效时间' : dateFormatter.format(date)
}

export function getScheduleSuggestions(nowInput: number | Date = Date.now()): ScheduleSuggestion[] {
  const now = new Date(nowInput)
  const earliest = now.getTime() + MIN_SCHEDULE_LEAD_MS
  let evening = atLocalTime(now, 0, 20)
  if (evening.getTime() < earliest) evening = atLocalTime(now, 1, 20)

  const tomorrow = atLocalTime(now, 1, 8)
  const daysUntilNextMonday = ((8 - now.getDay()) % 7) || 7
  const nextMonday = atLocalTime(now, daysUntilNextMonday, 8)

  return [
    {
      id: 'later',
      label: isSameLocalDay(evening, now) ? '今晚 20:00' : '明晚 20:00',
      detail: shortDateFormatter.format(evening),
      timestamp: evening.getTime(),
    },
    {
      id: 'tomorrow',
      label: '明早 08:00',
      detail: shortDateFormatter.format(tomorrow),
      timestamp: tomorrow.getTime(),
    },
    {
      id: 'next-week',
      label: '下周一 08:00',
      detail: shortDateFormatter.format(nextMonday),
      timestamp: nextMonday.getTime(),
    },
  ]
}

export function defaultCustomSchedule(now = Date.now()) {
  const rounded = Math.ceil((now + 30 * 60_000) / (30 * 60_000)) * (30 * 60_000)
  return Math.min(rounded, now + MAX_SCHEDULE_AHEAD_MS)
}

export function validateScheduledAt(timestamp: number, now = Date.now()): ScheduleValidation {
  if (!Number.isFinite(timestamp)) return { ok: false, error: '请选择有效的发送时间' }
  if (timestamp < now + MIN_SCHEDULE_LEAD_MS) return { ok: false, error: '定时发送至少需要提前 1 分钟' }
  if (timestamp > now + MAX_SCHEDULE_AHEAD_MS) return { ok: false, error: '定时发送最多可安排到一年内' }
  return { ok: true, timestamp: Math.floor(timestamp / 1_000) * 1_000 }
}
