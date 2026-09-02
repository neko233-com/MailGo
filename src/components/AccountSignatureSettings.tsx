import { useState } from 'react'
import { accountSignatureBytes, normalizeAccountSignature } from '../signature'
import type { MailAccount } from '../types'
import { Icon } from './Icon'

interface AccountSignatureSettingsProps {
  accounts: MailAccount[]
  initialAccountId?: string | null
  onSave: (accountId: string, signature: string) => Promise<string>
}

export function AccountSignatureSettings({ accounts, initialAccountId, onSave }: AccountSignatureSettingsProps) {
  const initialAccount = accounts.find((account) => account.id === initialAccountId) ?? accounts[0]
  const [accountId, setAccountId] = useState(initialAccount?.id ?? '')
  const [value, setValue] = useState(initialAccount?.signature ?? '')
  const [phase, setPhase] = useState<'idle' | 'saving' | 'saved' | 'error'>('idle')
  const [message, setMessage] = useState('')
  const selectedAccount = accounts.find((account) => account.id === accountId) ?? accounts[0]

  if (!selectedAccount) {
    return <section className="settings-signature is-empty"><Icon name="edit" size={17} /><span>添加邮箱后可设置账户签名</span></section>
  }

  const selectAccount = (nextId: string) => {
    const next = accounts.find((account) => account.id === nextId)
    if (!next) return
    setAccountId(next.id)
    setValue(next.signature ?? '')
    setPhase('idle')
    setMessage('')
  }

  const save = async () => {
    setPhase('saving')
    setMessage('')
    try {
      const normalized = normalizeAccountSignature(value)
      const saved = await onSave(selectedAccount.id, normalized)
      setValue(saved)
      setPhase('saved')
      setMessage('已保存；写信、回复和转发时自动加入')
    } catch (error) {
      setPhase('error')
      setMessage(error instanceof Error ? error.message : '签名保存失败')
    }
  }

  const signatureBytes = accountSignatureBytes(value)
  return <section className="settings-signature" aria-labelledby="account-signature-title">
    <div className="settings-signature-heading">
      <span><Icon name="edit" size={17} /><span><strong id="account-signature-title">账户签名</strong><small>按发件账户独立保存</small></span></span>
      <select aria-label="签名所属账户" value={selectedAccount.id} onChange={(event) => selectAccount(event.target.value)}>
        {accounts.map((account) => <option key={account.id} value={account.id}>{account.label}</option>)}
      </select>
    </div>
    <textarea maxLength={8192} value={value} onChange={(event) => { setValue(event.target.value); setPhase('idle'); setMessage('') }} placeholder="例如：姓名、职位、联系电话" />
    <div className={`settings-signature-foot is-${phase}`}>
      <span>{message || `${signatureBytes.toLocaleString()} / 8,192 字节`}</span>
      <button type="button" onClick={() => { void save() }} disabled={phase === 'saving'}>{phase === 'saving' ? '保存中…' : '保存签名'}</button>
    </div>
  </section>
}
