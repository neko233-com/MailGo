import { AnimatePresence, motion } from 'motion/react'
import { useEffect, useRef, useState, type FormEvent } from 'react'
import {
  MAX_SCHEDULE_AHEAD_MS,
  MIN_SCHEDULE_LEAD_MS,
  defaultCustomSchedule,
  formatScheduledAt,
  getScheduleSuggestions,
  toLocalDateTimeInputValue,
  validateScheduledAt,
} from '../scheduleSend'
import { Icon } from './Icon'

type ScheduleSendControlProps = {
  disabled: boolean
  label: string
  onSendNow: () => void
  onSchedule: (timestamp: number) => void
}

export function ScheduleSendControl({ disabled, label, onSendNow, onSchedule }: ScheduleSendControlProps) {
  const [open, setOpen] = useState(false)
  const [customValue, setCustomValue] = useState(() => toLocalDateTimeInputValue(defaultCustomSchedule()))
  const [error, setError] = useState('')
  const firstOptionRef = useRef<HTMLButtonElement>(null)
  const now = Date.now()
  const suggestions = getScheduleSuggestions(now)

  useEffect(() => {
    if (open) firstOptionRef.current?.focus()
  }, [open])

  const toggleMenu = () => {
    if (disabled) return
    setError('')
    setCustomValue(toLocalDateTimeInputValue(defaultCustomSchedule()))
    setOpen((value) => !value)
  }

  const schedule = (timestamp: number) => {
    const result = validateScheduledAt(timestamp)
    if (!result.ok) {
      setError(result.error)
      return
    }
    setOpen(false)
    onSchedule(result.timestamp)
  }

  const submitCustom = (event: FormEvent) => {
    event.preventDefault()
    schedule(new Date(customValue).getTime())
  }

  return <div
    className="schedule-send-control"
    onBlur={(event) => {
      if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false)
    }}
  >
    <div className="compose-send-split">
      <button type="button" className="gradient-button compose-send-now" onClick={onSendNow} disabled={disabled}>{label}<Icon name="send" size={17} /></button>
      <button type="button" className="compose-schedule-toggle" aria-label="定时发送" title="定时发送" aria-haspopup="dialog" aria-expanded={open} aria-controls="compose-schedule-menu" onClick={toggleMenu} disabled={disabled}><Icon name="clock" size={16} /></button>
    </div>
    <AnimatePresence>
      {open && <motion.div
        id="compose-schedule-menu"
        className="compose-schedule-menu"
        role="dialog"
        aria-label="选择定时发送时间"
        initial={{ opacity: 0, y: 6, scale: .98 }}
        animate={{ opacity: 1, y: 0, scale: 1 }}
        exit={{ opacity: 0, y: 4, scale: .98 }}
        onKeyDown={(event) => {
          if (event.key !== 'Escape') return
          event.preventDefault()
          event.stopPropagation()
          setOpen(false)
        }}
      >
        <div className="compose-schedule-heading"><span><Icon name="clock" size={16} />定时发送</span><button type="button" onClick={() => setOpen(false)} aria-label="关闭定时发送"><Icon name="close" size={15} /></button></div>
        <div className="compose-schedule-options">
          {suggestions.map((suggestion, index) => <button ref={index === 0 ? firstOptionRef : undefined} type="button" key={suggestion.id} onClick={() => schedule(suggestion.timestamp)}><span><strong>{suggestion.label}</strong><small>{suggestion.detail}</small></span><Icon name="forward" size={14} /></button>)}
        </div>
        <form noValidate onSubmit={submitCustom}>
          <label>自定义时间<input type="datetime-local" value={customValue} min={toLocalDateTimeInputValue(now + MIN_SCHEDULE_LEAD_MS)} max={toLocalDateTimeInputValue(now + MAX_SCHEDULE_AHEAD_MS)} onChange={(event) => { setCustomValue(event.target.value); setError('') }} /></label>
          {error && <p role="alert">{error}</p>}
          <button type="submit">安排发送</button>
        </form>
        <small className="compose-schedule-note">邮件会加密保存在本机发件箱；关窗、离线或电脑休眠都不会丢失，恢复运行后自动补发。</small>
      </motion.div>}
    </AnimatePresence>
  </div>
}
