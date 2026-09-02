import { AnimatePresence, motion } from 'motion/react'
import { useEffect, useMemo, useRef, useState } from 'react'
import { invoke } from '../lib/ipc'
import { activeRecipientQuery, applyRecipientSuggestion, filterRecipientDirectory, recipientEmails } from '../recipients'
import type { NativeRecipientSuggestion, NativeRecipientSuggestionResponse } from '../types'
import { Icon } from './Icon'

const demoDirectory: NativeRecipientSuggestion[] = [
  { name: 'Alice Chen', email: 'alice.chen@example.invalid', frequency: 12, lastSeen: '2026-08-28T09:00:00Z' },
  { name: 'Alex Morgan', email: 'alex.morgan@example.invalid', frequency: 7, lastSeen: '2026-08-25T09:00:00Z' },
  { name: 'Design Review', email: 'design.review@example.invalid', frequency: 4, lastSeen: '2026-08-20T09:00:00Z' },
]

type RecipientInputProps = {
  fieldId: string
  label: string
  value: string
  onChange: (value: string) => void
  accountId?: string
  senderEmail?: string
  placeholder: string
  autoFocus?: boolean
}

export function RecipientInput({ fieldId, label, value, onChange, accountId, senderEmail, placeholder, autoFocus = false }: RecipientInputProps) {
  const inputRef = useRef<HTMLInputElement>(null)
  const [focused, setFocused] = useState(false)
  const [touched, setTouched] = useState(false)
  const [status, setStatus] = useState<'idle' | 'loading' | 'ready' | 'error'>('idle')
  const [suggestions, setSuggestions] = useState<NativeRecipientSuggestion[]>([])
  const [activeIndex, setActiveIndex] = useState(0)
  const [indexing, setIndexing] = useState(false)
  const [dismissedQuery, setDismissedQuery] = useState('')
  const query = activeRecipientQuery(value)
  const excludedEmails = useMemo(() => {
    const excluded = recipientEmails(value)
    if (senderEmail) excluded.add(senderEmail.trim().toLowerCase())
    return excluded
  }, [senderEmail, value])

  useEffect(() => {
    if (!focused || !touched || !query || query === dismissedQuery) {
      setStatus('idle')
      setSuggestions([])
      setIndexing(false)
      return
    }
    let cancelled = false
    setStatus('loading')
    setSuggestions([])
    const timer = window.setTimeout(() => {
      const request = window.ipc?.postMessage && accountId
        ? invoke<NativeRecipientSuggestionResponse>('contacts.suggest', { accountId, query, limit: 8 }, 15_000)
        : Promise.resolve<NativeRecipientSuggestionResponse>({
            suggestions: filterRecipientDirectory(demoDirectory, query, excludedEmails, 8),
            truncated: false,
            indexing: false,
          })
      void request.then((result) => {
        if (cancelled) return
        const safeSuggestions = filterRecipientDirectory(result.suggestions ?? [], query, excludedEmails, 8)
        setSuggestions(safeSuggestions)
        setActiveIndex(0)
        setIndexing(Boolean(result.indexing))
        setStatus('ready')
      }).catch(() => {
        if (cancelled) return
        setSuggestions([])
        setIndexing(false)
        setStatus('error')
      })
    }, 120)
    return () => {
      cancelled = true
      window.clearTimeout(timer)
    }
  }, [accountId, dismissedQuery, excludedEmails, focused, query, touched])

  const choose = (suggestion: NativeRecipientSuggestion) => {
    onChange(applyRecipientSuggestion(value, suggestion))
    setSuggestions([])
    setStatus('idle')
    setDismissedQuery('')
    inputRef.current?.focus()
  }

  const menuVisible = focused && touched && Boolean(query) && (status !== 'idle' || suggestions.length > 0)
  const listboxId = `${fieldId}-suggestions`

  return (
    <div className="compose-recipient-label">
      <label htmlFor={fieldId}>{label}</label>
      <span className="compose-recipient-field">
        <input
          ref={inputRef}
          id={fieldId}
          autoFocus={autoFocus}
          value={value}
          onChange={(event) => {
            setTouched(true)
            setDismissedQuery('')
            onChange(event.target.value)
          }}
          onFocus={() => setFocused(true)}
          onBlur={() => setFocused(false)}
          onKeyDown={(event) => {
            if (!menuVisible) return
            if (event.key === 'ArrowDown' && suggestions.length) {
              event.preventDefault()
              setActiveIndex((current) => (current + 1) % suggestions.length)
            } else if (event.key === 'ArrowUp' && suggestions.length) {
              event.preventDefault()
              setActiveIndex((current) => (current - 1 + suggestions.length) % suggestions.length)
            } else if ((event.key === 'Enter' || event.key === 'Tab') && suggestions[activeIndex]) {
              event.preventDefault()
              choose(suggestions[activeIndex])
            } else if (event.key === 'Escape') {
              event.preventDefault()
              event.stopPropagation()
              setDismissedQuery(query)
              setSuggestions([])
              setStatus('idle')
            }
          }}
          placeholder={placeholder}
          role="combobox"
          aria-autocomplete="list"
          aria-expanded={menuVisible}
          aria-controls={menuVisible ? listboxId : undefined}
          aria-activedescendant={menuVisible && suggestions[activeIndex] ? `${listboxId}-${activeIndex}` : undefined}
        />
        <AnimatePresence>
          {menuVisible && <motion.span className="recipient-suggestions" id={listboxId} role="listbox" aria-label={`${label}建议`} initial={{ opacity: 0, y: -4 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0, y: -4 }}>
            {status === 'loading' && <span className="recipient-suggestion-status" role="status"><span className="loading-spinner loading-spinner-small" aria-hidden="true" />正在检索本机往来联系人…</span>}
            {status === 'error' && <span className="recipient-suggestion-status is-error"><Icon name="info" size={14} />本机联系人暂不可用</span>}
            {status === 'ready' && suggestions.length === 0 && <span className="recipient-suggestion-status"><Icon name="search" size={14} />未找到匹配的本机联系人</span>}
            {suggestions.map((suggestion, index) => <button
              id={`${listboxId}-${index}`}
              role="option"
              aria-selected={index === activeIndex}
              className={index === activeIndex ? 'is-active' : ''}
              type="button"
              key={suggestion.email.toLowerCase()}
              onMouseDown={(event) => event.preventDefault()}
              onMouseEnter={() => setActiveIndex(index)}
              onClick={() => choose(suggestion)}
            ><span className="recipient-suggestion-avatar">{(suggestion.name || suggestion.email).trim().slice(0, 1).toUpperCase()}</span><span><strong>{suggestion.name || suggestion.email}</strong><small>{suggestion.email}</small></span><em>往来 {Math.max(1, suggestion.frequency)} 次</em></button>)}
            {indexing && <span className="recipient-indexing-note"><Icon name="rotate" size={13} />历史联系人仍在后台整理，结果会逐步补全</span>}
          </motion.span>}
        </AnimatePresence>
      </span>
    </div>
  )
}
