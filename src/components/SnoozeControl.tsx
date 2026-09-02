import { motion, AnimatePresence } from 'motion/react'
import { type FormEvent, useEffect, useMemo, useRef, useState } from 'react'
import { Icon } from './Icon'
import { defaultCustomSnoozeTime, snoozeSuggestions, toLocalDateTimeInput, validateSnoozeTime } from '../snooze'

type SnoozeControlProps = {
  disabled?: boolean
  onSnooze: (timestamp: number) => Promise<void> | void
}

export function SnoozeControl({ disabled = false, onSnooze }: SnoozeControlProps) {
  const [open, setOpen] = useState(false)
  const [customValue, setCustomValue] = useState(() => toLocalDateTimeInput(defaultCustomSnoozeTime()))
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const firstOptionRef = useRef<HTMLButtonElement>(null)
  const suggestions = useMemo(() => snoozeSuggestions(), [open])

  useEffect(() => {
    if (!open) return
    setCustomValue(toLocalDateTimeInput(defaultCustomSnoozeTime()))
    setError('')
    const timer = window.setTimeout(() => firstOptionRef.current?.focus(), 0)
    return () => window.clearTimeout(timer)
  }, [open])

  const commit = async (timestamp: number) => {
    const validation = validateSnoozeTime(timestamp)
    if (validation) {
      setError(validation)
      return
    }
    setBusy(true)
    try {
      await onSnooze(timestamp)
      setOpen(false)
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '暂时无法稍后处理这封邮件')
    } finally {
      setBusy(false)
    }
  }

  const submitCustom = (event: FormEvent) => {
    event.preventDefault()
    void commit(new Date(customValue).getTime())
  }

  return (
    <div className="snooze-control menu-anchor" onBlur={(event) => {
      if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false)
    }}>
      <button type="button" className="icon-button snooze-trigger" aria-label="稍后处理" title="稍后处理" aria-expanded={open} disabled={disabled || busy} onClick={() => setOpen((value) => !value)}>
        <Icon name="clock" size={18} />
      </button>
      <AnimatePresence>
        {open && <motion.div className="snooze-menu" role="dialog" aria-label="选择稍后处理时间" initial={{ opacity: 0, y: -5, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: -4, scale: 0.98 }} transition={{ duration: 0.14 }} onKeyDown={(event) => {
          if (event.key === 'Escape') {
            event.preventDefault()
            event.stopPropagation()
            setOpen(false)
          }
        }}>
          <div className="snooze-heading"><span><Icon name="clock" size={15} />稍后处理</span><button type="button" aria-label="关闭稍后处理菜单" onClick={() => setOpen(false)}><Icon name="close" size={15} /></button></div>
          <div className="snooze-options">
            {suggestions.map((suggestion, index) => <button ref={index === 0 ? firstOptionRef : undefined} key={suggestion.id} type="button" disabled={busy} onClick={() => { void commit(suggestion.timestamp) }}><Icon name={suggestion.id === 'weekend' ? 'bell' : 'clock'} size={16} /><span><strong>{suggestion.label}</strong><small>{suggestion.detail}</small></span></button>)}
          </div>
          <form noValidate onSubmit={submitCustom}>
            <label>自定义时间<input type="datetime-local" value={customValue} disabled={busy} onChange={(event) => { setCustomValue(event.target.value); setError('') }} /></label>
            {error && <p role="alert">{error}</p>}
            <button type="submit" disabled={busy}>{busy ? '保存中…' : '稍后提醒我'}</button>
          </form>
          <small className="snooze-note">本机加密保存；到点自动回到收件箱。</small>
        </motion.div>}
      </AnimatePresence>
    </div>
  )
}
