import { motion } from 'motion/react'
import { providerDefinitions } from '../data'
import type { NativeConnectionDiagnostic, NativeConnectionDiagnosticChannel, Provider, ProviderDefinition } from '../types'
import { Icon, type IconName } from './Icon'
import { ProviderMark } from './ProviderMark'
import { TooltipButton } from './TooltipButton'

export type DeviceFlowState = { sessionId: string; userCode: string; verificationUri: string; message?: string; retryAfter: number; status: 'pending' | 'complete' | 'error' }
export type ConnectionDiagnosticViewState =
  | { phase: 'checking' }
  | { phase: 'ready'; result: NativeConnectionDiagnostic }
  | { phase: 'error'; message: string }

const diagnosticStatusLabels = {
  ok: '连接正常',
  authentication: '需要重新授权',
  'rate-limit': '服务商暂时限流',
  network: '网络不可达',
  tls: '安全连接失败',
  provider: '服务商拒绝连接',
} as const

function ConnectionDiagnosticChannelRow({ label, icon, phase, channel }: { label: string; icon: IconName; phase: ConnectionDiagnosticViewState['phase'] | 'idle'; channel?: NativeConnectionDiagnosticChannel }) {
  const status = channel?.status ?? phase
  const labelText = channel ? diagnosticStatusLabels[channel.status] : phase === 'checking' ? '检测中…' : '尚未检测'
  return <div className={`connection-diagnostic-channel is-${status}`}><span className="connection-diagnostic-icon"><Icon name={icon} size={17} /></span><span><strong>{label}</strong><small>{labelText}{channel ? ` · ${Math.max(0, Math.round(channel.latencyMs))} ms` : ''}</small></span>{channel?.ok && <Icon name="checkCircle" size={17} />}{phase === 'checking' && !channel && <span className="loading-spinner loading-spinner-small" aria-hidden="true" />}</div>
}

function ConnectionDiagnosticCard({ diagnostic, disabled, onDiagnose }: { diagnostic?: ConnectionDiagnosticViewState; disabled: boolean; onDiagnose: () => void }) {
  const phase = diagnostic?.phase ?? 'idle'
  const result = diagnostic?.phase === 'ready' ? diagnostic.result : undefined
  return <section className={`connection-diagnostic is-${phase}`} aria-label="收发连接检测"><div className="connection-diagnostic-heading"><span><Icon name="cloud" size={16} /><span><strong>收发连接检测</strong><small>只登录并发送 NOOP，不会发送邮件</small></span></span><button type="button" onClick={onDiagnose} disabled={disabled || phase === 'checking'}><Icon name="rotate" size={14} />{phase === 'checking' ? '检测中…' : '开始检测'}</button></div><div className="connection-diagnostic-grid"><ConnectionDiagnosticChannelRow label="IMAP 收件" icon="inbox" phase={phase} channel={result?.incoming} /><ConnectionDiagnosticChannelRow label="SMTP 发件" icon="send" phase={phase} channel={result?.outgoing} /></div>{diagnostic?.phase === 'error' && <p role="alert"><Icon name="info" size={14} />{diagnostic.message}</p>}</section>
}

interface AccountModalProps {
  editingAccountId: string | null
  provider: Provider
  setProvider: (provider: Provider) => void
  providerDefinition: ProviderDefinition
  accountEmail: string
  setAccountEmail: (value: string) => void
  authorizationCode: string
  setAuthorizationCode: (value: string) => void
  showAuthorizationCode: boolean
  setShowAuthorizationCode: (value: boolean) => void
  customImapHost: string
  setCustomImapHost: (value: string) => void
  customImapPort: string
  setCustomImapPort: (value: string) => void
  customImapSecurity: string
  setCustomImapSecurity: (value: string) => void
  customSmtpHost: string
  setCustomSmtpHost: (value: string) => void
  customSmtpPort: string
  setCustomSmtpPort: (value: string) => void
  customSmtpSecurity: string
  setCustomSmtpSecurity: (value: string) => void
  customAuthentication: string
  setCustomAuthentication: (value: string) => void
  deviceFlow: DeviceFlowState | null
  diagnostic?: ConnectionDiagnosticViewState
  isBusy: boolean
  onClose: () => void
  onOpenProvider: () => void
  onCopy: () => void
  onAdd: () => void
  onRemove: () => void
  onDiagnose: () => void
}

export function AccountModal({ editingAccountId, provider, setProvider, providerDefinition, accountEmail, setAccountEmail, authorizationCode, setAuthorizationCode, showAuthorizationCode, setShowAuthorizationCode, customImapHost, setCustomImapHost, customImapPort, setCustomImapPort, customImapSecurity, setCustomImapSecurity, customSmtpHost, setCustomSmtpHost, customSmtpPort, setCustomSmtpPort, customSmtpSecurity, setCustomSmtpSecurity, customAuthentication, setCustomAuthentication, deviceFlow, diagnostic, isBusy, onClose, onOpenProvider, onCopy, onAdd, onRemove, onDiagnose }: AccountModalProps) {
  const isOAuth = customAuthentication === 'oauth2'
  const credentialLabel = isBusy ? '正在保存…' : isOAuth ? '手动授权码（可选）' : providerDefinition.requiresAuthCode ? '授权码' : '登录凭据'
  const guideTitle = isOAuth ? '如何完成安全授权？' : '如何获取授权码？'
  const guide = isOAuth
    ? provider === 'outlook'
      ? ['打开 Microsoft 设备验证页面', '输入 MailGo 显示的设备代码', '完成账户授权后返回 MailGo']
      : ['点击开始授权并打开服务商登录页', '在服务商页面确认 MailGo 的访问权限', '完成后返回 MailGo，系统会自动保存令牌']
    : providerDefinition.guide
  return <motion.div className="modal-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}><motion.div className="account-modal" initial={{ opacity: 0, y: 14, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 8, scale: 0.98 }} role="dialog" aria-modal="true" aria-labelledby="account-modal-title"><div className="modal-header"><div><Icon name="user" size={21} /><h2 id="account-modal-title">{editingAccountId ? '重新授权账户' : '添加账户'}</h2></div><TooltipButton label="关闭" onClick={onClose} disabled={isBusy}><Icon name="close" size={19} /></TooltipButton></div><div className="account-modal-body"><div className="provider-chooser">{providerDefinitions.map((item) => <button key={item.id} type="button" className={`provider-option ${provider === item.id ? 'is-selected' : ''}`} onClick={() => setProvider(item.id)} disabled={isBusy}><ProviderMark provider={item.id} size="md" /><span><strong>{item.label}</strong><small>{item.description}</small></span>{provider === item.id && <Icon name="checkCircle" size={19} />}</button>)}</div><div className="account-form"><label>邮箱地址<input type="email" value={accountEmail} onChange={(event) => setAccountEmail(event.target.value)} placeholder={provider === 'qq' ? 'yourname@qq.com' : 'name@example.com'} autoFocus disabled={isBusy} /></label>{(provider === 'google' || provider === 'outlook') && <label>认证方式<select value={customAuthentication} onChange={(event) => setCustomAuthentication(event.target.value)} disabled={isBusy}><option value="oauth2">OAuth2 安全授权</option>{provider === 'google' && <option value="app-password">应用专用密码</option>}</select></label>}<label><span className="label-with-action">{credentialLabel}<button type="button" onClick={onOpenProvider} disabled={isBusy}>{isOAuth ? '打开授权页面' : '如何获取授权码？'} <Icon name="link" size={13} /></button></span><span className="secret-input"><input type={showAuthorizationCode ? 'text' : 'password'} value={authorizationCode} onChange={(event) => setAuthorizationCode(event.target.value)} placeholder={isOAuth ? 'OAuth 授权完成后无需粘贴' : '粘贴邮箱授权码'} disabled={isBusy} /><button type="button" onClick={() => setShowAuthorizationCode(!showAuthorizationCode)} aria-label={showAuthorizationCode ? '隐藏授权码' : '显示授权码'} disabled={isBusy}><Icon name={showAuthorizationCode ? 'eyeSlash' : 'eye'} size={17} /></button><button type="button" onClick={onCopy} aria-label="复制授权码" disabled={isBusy}><Icon name="copy" size={17} /></button></span></label>{deviceFlow && <div className="device-flow-box"><div className="device-flow-heading"><span><Icon name="shieldCheck" size={16} />Outlook 设备授权</span><strong>{deviceFlow.status === 'complete' ? '已完成' : deviceFlow.status === 'error' ? '需要重试' : '等待验证'}</strong></div><code>{deviceFlow.userCode}</code><p>{deviceFlow.status === 'complete' ? '设备验证已完成，可以开始同步。' : (deviceFlow.message || '请打开验证页完成 Microsoft 账户授权。')}</p><small>{deviceFlow.verificationUri}</small>{deviceFlow.status !== 'complete' && <button type="button" onClick={onOpenProvider} disabled={isBusy}>{deviceFlow.status === 'error' ? '重新开始授权' : '重新打开验证页'} <Icon name="link" size={13} /></button>}</div>}{provider === 'other' && <div className="custom-transport-fields"><div className="transport-heading"><Icon name="settings" size={15} />自定义服务器</div><div className="transport-row"><label>IMAP 主机<input value={customImapHost} onChange={(event) => setCustomImapHost(event.target.value)} placeholder="imap.example.com" disabled={isBusy} /></label><label>端口<input type="number" min="1" max="65535" value={customImapPort} onChange={(event) => setCustomImapPort(event.target.value)} disabled={isBusy} /></label><label>安全<input value={customImapSecurity} onChange={(event) => setCustomImapSecurity(event.target.value)} placeholder="tls / starttls" disabled={isBusy} /></label></div><div className="transport-row"><label>SMTP 主机<input value={customSmtpHost} onChange={(event) => setCustomSmtpHost(event.target.value)} placeholder="smtp.example.com" disabled={isBusy} /></label><label>端口<input type="number" min="1" max="65535" value={customSmtpPort} onChange={(event) => setCustomSmtpPort(event.target.value)} disabled={isBusy} /></label><label>安全<input value={customSmtpSecurity} onChange={(event) => setCustomSmtpSecurity(event.target.value)} placeholder="tls / starttls" disabled={isBusy} /></label></div><label>认证方式<select value={customAuthentication} onChange={(event) => setCustomAuthentication(event.target.value)} disabled={isBusy}><option value="password">密码 / 授权码</option><option value="app-password">应用专用密码</option><option value="oauth2">OAuth2 Bearer Token</option></select></label></div>}{editingAccountId && <ConnectionDiagnosticCard diagnostic={diagnostic} disabled={isBusy} onDiagnose={onDiagnose} />}<div className="guide-box"><div className="guide-heading"><span><Icon name="key" size={17} />{guideTitle}</span><em>{providerDefinition.label}</em></div>{guide.map((step, index) => <div className="guide-step" key={step}><span className="step-number">{index + 1}</span><span><strong>{step}</strong><small>{isOAuth ? (index === 0 ? 'MailGo 会在本机发起安全授权流程' : index === 1 ? '只授予邮件同步所需的账户权限' : '令牌仅保存到本机系统安全存储') : (index === 0 ? `登录 ${providerDefinition.label}，打开设置页面` : index === 1 ? '找到第三方客户端或账户安全选项' : '复制生成的授权凭据，返回此处粘贴')}</small></span>{index === 0 && <button type="button" onClick={onOpenProvider} disabled={isBusy}>{isOAuth ? '开始授权' : '前往设置'} <Icon name="link" size={13} /></button>}</div>)}</div></div></div><div className="modal-footer"><span><Icon name="shieldCheck" size={17} />凭据仅存本机 · 邮件后台同步</span><div>{editingAccountId && <button className="danger-button" type="button" onClick={onRemove} disabled={isBusy}><Icon name="trash" size={16} />移除账户</button>}<button className="secondary-button" type="button" onClick={onClose} disabled={isBusy}>取消</button><button className="gradient-button" type="button" onClick={onAdd} disabled={isBusy}><Icon name="rotate" size={17} />{isBusy ? '正在保存…' : editingAccountId ? '保存并进入' : '添加并进入'}</button></div></div></motion.div></motion.div>
}
