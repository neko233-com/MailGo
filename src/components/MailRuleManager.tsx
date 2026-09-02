import { motion, useReducedMotion } from 'motion/react'
import { useEffect, useMemo, useState } from 'react'
import { normalizeRuleValue } from '../mailRules'
import type { MailAccount, MailRuleKind, NativeMailRule } from '../types'
import { Icon } from './Icon'

interface MailRuleManagerProps {
  accounts: MailAccount[]
  rules: NativeMailRule[]
  initialAccountId?: string | null
  busyKey: string | null
  externalError?: string
  onAdd: (accountId: string | undefined, kind: MailRuleKind, value: string) => Promise<void>
  onRemove: (rule: NativeMailRule) => Promise<void>
  onClose: () => void
}

function ruleKindLabel(kind: MailRuleKind) {
  return kind === 'sender' ? '发件人' : '域名'
}

export function MailRuleManager({ accounts, rules, initialAccountId, busyKey, externalError, onAdd, onRemove, onClose }: MailRuleManagerProps) {
  const prefersReducedMotion = useReducedMotion()
  const [accountId, setAccountId] = useState(initialAccountId ?? '')
  const [kind, setKind] = useState<MailRuleKind>('sender')
  const [value, setValue] = useState('')
  const [validationError, setValidationError] = useState('')
  const [externalErrorDismissed, setExternalErrorDismissed] = useState(false)
  const accountsById = useMemo(() => new Map(accounts.map((account) => [account.id, account])), [accounts])

  useEffect(() => setExternalErrorDismissed(false), [externalError])

  useEffect(() => {
    const handleEscape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || busyKey) return
      event.preventDefault()
      onClose()
    }
    window.addEventListener('keydown', handleEscape)
    return () => window.removeEventListener('keydown', handleEscape)
  }, [busyKey, onClose])

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    try {
      const normalized = normalizeRuleValue(kind, value)
      setValidationError('')
      await onAdd(accountId || undefined, kind, normalized)
      setValue('')
    } catch (error) {
      setValidationError(error instanceof Error ? error.message : '规则保存失败')
    }
  }

  return (
    <motion.div className="modal-backdrop mail-rule-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget && !busyKey) onClose() }}>
      <motion.section
        className="mail-rule-modal"
        initial={prefersReducedMotion ? false : { opacity: 0, y: 14, scale: 0.985 }}
        animate={{ opacity: 1, y: 0, scale: 1 }}
        exit={prefersReducedMotion ? { opacity: 0 } : { opacity: 0, y: 8, scale: 0.985 }}
        role="dialog"
        aria-modal="true"
        aria-labelledby="mail-rule-title"
      >
        <header className="modal-header">
          <div><span className="mail-rule-title-icon"><Icon name="shieldCheck" size={20} /></span><span><h2 id="mail-rule-title">屏蔽规则</h2><small>加密保存在这台电脑</small></span></div>
          <button type="button" className="icon-button" aria-label="关闭屏蔽规则" disabled={Boolean(busyKey)} onClick={onClose}><Icon name="close" size={18} /></button>
        </header>

        <form className="mail-rule-form" onSubmit={submit}>
          <div className="mail-rule-form-heading"><strong>添加规则</strong><span>命中后从普通列表隐藏，在“推广”中仍可查看</span></div>
          <div className="mail-rule-form-grid">
            <label><span>作用账户</span><select aria-label="规则作用账户" value={accountId} onChange={(event) => setAccountId(event.target.value)}><option value="">所有账户</option>{accounts.map((account) => <option key={account.id} value={account.id}>{account.label} · {account.email}</option>)}</select></label>
            <label><span>规则类型</span><select aria-label="屏蔽规则类型" value={kind} onChange={(event) => setKind(event.target.value as MailRuleKind)}><option value="sender">完整发件人</option><option value="domain">发件域名</option></select></label>
            <label className="mail-rule-value"><span>{kind === 'sender' ? '发件人邮箱' : '发件域名'}</span><div><Icon name={kind === 'sender' ? 'at' : 'link'} size={16} /><input aria-label={kind === 'sender' ? '发件人邮箱' : '发件域名'} value={value} maxLength={320} autoFocus autoComplete="off" spellCheck={false} placeholder={kind === 'sender' ? 'newsletter@example.com' : 'example.com'} onChange={(event) => { setValue(event.target.value); setValidationError(''); setExternalErrorDismissed(true) }} /></div></label>
            <button className="gradient-button mail-rule-add" type="submit" disabled={Boolean(busyKey) || !value.trim()}>{busyKey === 'add' ? <span className="loading-spinner loading-spinner-small" aria-hidden="true" /> : <Icon name="add" size={16} />}{busyKey === 'add' ? '保存中…' : '添加屏蔽'}</button>
          </div>
          {(validationError || (externalError && !externalErrorDismissed)) && <p className="mail-rule-error" role="alert"><Icon name="info" size={14} />{validationError || externalError}</p>}
        </form>

        <div className="mail-rule-list-heading"><span><strong>当前规则</strong><small>{rules.length} / 256</small></span><span>发件域名规则同时匹配其子域名</span></div>
        <div className="mail-rule-list" aria-label="当前屏蔽规则">
          {rules.length === 0 ? <div className="mail-rule-empty"><span><Icon name="shield" size={22} /></span><strong>还没有手动屏蔽规则</strong><p>可从任意邮件的“更多”菜单一键添加。</p></div> : rules.map((rule) => {
            const account = rule.accountId ? accountsById.get(rule.accountId) : undefined
            const removing = busyKey === rule.id
            return <div className="mail-rule-item" key={rule.id}><span className="mail-rule-kind"><Icon name={rule.kind === 'sender' ? 'user' : 'link'} size={16} /></span><span className="mail-rule-copy"><strong title={rule.value}>{rule.value}</strong><small>{ruleKindLabel(rule.kind)} · {rule.accountId ? (account ? account.label : '已移除账户') : '所有账户'}</small></span><button type="button" aria-label={`移除规则 ${rule.value}`} disabled={Boolean(busyKey)} onClick={() => { void onRemove(rule) }}>{removing ? <span className="loading-spinner loading-spinner-small" aria-hidden="true" /> : <Icon name="trash" size={15} />}<span>{removing ? '移除中' : '移除'}</span></button></div>
          })}
        </div>
        <footer className="mail-rule-footer"><span><Icon name="lock" size={14} />规则不会上传或写入邮件服务器</span><button type="button" onClick={onClose} disabled={Boolean(busyKey)}>完成</button></footer>
      </motion.section>
    </motion.div>
  )
}
