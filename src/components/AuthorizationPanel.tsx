import { motion } from 'motion/react'
import type { MailAccount } from '../types'
import { Icon } from './Icon'
import { ProviderMark } from './ProviderMark'
import { TooltipButton } from './TooltipButton'

interface AuthorizationPanelProps {
  accounts: MailAccount[]
  isMobileOpen: boolean
  reduceMotion: boolean
  providerLabel: string
  onClose: () => void
  onManageAuthorization: () => void
  onEditAccount: (account: MailAccount) => void
  onOpenProvider: () => void
}

export function AuthorizationPanel({
  accounts,
  isMobileOpen,
  reduceMotion,
  providerLabel,
  onClose,
  onManageAuthorization,
  onEditAccount,
  onOpenProvider,
}: AuthorizationPanelProps) {
  return <motion.aside className={`auth-panel ${isMobileOpen ? 'is-mobile-open' : ''}`} initial={reduceMotion ? false : { x: 24, opacity: 0 }} animate={{ x: 0, opacity: 1 }} exit={{ x: 24, opacity: 0 }} transition={{ duration: 0.2 }}>
    <div className="auth-panel-header"><div><Icon name="key" size={20} /><strong>授权码助手</strong></div><TooltipButton label="关闭授权码助手" onClick={onClose}><Icon name="close" size={18} /></TooltipButton></div>
    <div className="auth-tabs"><button type="button" className="is-active"><Icon name="lock" size={16} />授权码</button><button type="button" onClick={onManageAuthorization}><Icon name="settings" size={16} />设置</button></div>
    <div className="auth-card"><div className="auth-illustration"><Icon name="shieldCheck" size={34} /></div><h2>快速获取授权码</h2><p>用于第三方服务登录验证</p><button className="gradient-button" type="button" onClick={onManageAuthorization}><Icon name="copy" size={17} />管理授权码</button><div className="auth-validity"><Icon name="clock" size={16} />授权码仅保存在本机安全存储</div></div>
    <div className="auth-panel-section"><div className="panel-section-title">账户</div>{accounts.length > 0 ? accounts.map((account) => <button type="button" className="auth-account-row" key={account.id} onClick={() => onEditAccount(account)}><ProviderMark provider={account.provider} size="sm" /><span><strong>{account.label}</strong><small>{account.email}</small></span><span className="auth-chevron">›</span></button>) : <div className="auth-account-empty"><Icon name="user" size={16} />添加账户后可在这里快速重新授权</div>}</div>
    <div className="auth-note"><Icon name="info" size={18} /><span>授权码仅用于登录验证<br />不会存储或同步到云端</span></div>
    <div className="auth-panel-foot"><button type="button" onClick={onOpenProvider}><Icon name="link" size={15} />打开 {providerLabel} 设置</button></div>
  </motion.aside>
}
