import { motion } from 'motion/react'
import type { ChangeEvent, RefObject } from 'react'
import type { MailAccount, ThemeMode } from '../types'
import { AccountSignatureSettings } from './AccountSignatureSettings'
import { Icon } from './Icon'
import { TooltipButton } from './TooltipButton'

export type DisplayDensity = 'compact' | 'comfortable'
export type UndoSendSeconds = 0 | 5 | 10 | 20 | 30
const UNDO_SEND_OPTIONS: readonly UndoSendSeconds[] = [0, 5, 10, 20, 30]

interface SettingsPopoverProps {
  theme: ThemeMode
  displayDensity: DisplayDensity
  viewportRequiresCompactDensity: boolean
  undoSendSeconds: UndoSendSeconds
  customCss: string
  removedUnsafeCustomCss: boolean
  accounts: MailAccount[]
  selectedAccountId: string | null
  mailRuleCount: number
  minimizeToTray: boolean
  remoteImagesEnabled: boolean
  notificationsEnabled: boolean
  isImporting: boolean
  importInputRef: RefObject<HTMLInputElement | null>
  onClose: () => void
  onToggleTheme: () => void
  onToggleDensity: () => void
  onUndoSendSecondsChange: (value: UndoSendSeconds) => void
  onCustomCssChange: (value: string) => void
  onSaveSignature: (accountId: string, value: string) => Promise<string>
  onOpenMailRules: () => void
  onToggleMinimizeToTray: () => void
  onToggleRemoteImages: () => void
  onToggleNotifications: () => void
  onExportAccounts: () => void
  onImportAccounts: (event: ChangeEvent<HTMLInputElement>) => void | Promise<void>
}

export function SettingsPopover({
  theme,
  displayDensity,
  viewportRequiresCompactDensity,
  undoSendSeconds,
  customCss,
  removedUnsafeCustomCss,
  accounts,
  selectedAccountId,
  mailRuleCount,
  minimizeToTray,
  remoteImagesEnabled,
  notificationsEnabled,
  isImporting,
  importInputRef,
  onClose,
  onToggleTheme,
  onToggleDensity,
  onUndoSendSecondsChange,
  onCustomCssChange,
  onSaveSignature,
  onOpenMailRules,
  onToggleMinimizeToTray,
  onToggleRemoteImages,
  onToggleNotifications,
  onExportAccounts,
  onImportAccounts,
}: SettingsPopoverProps) {
  return <motion.div className="settings-popover" initial={{ opacity: 0, y: 8 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0, y: 8 }}>
    <div className="settings-title"><span><Icon name="settings" size={17} />偏好设置</span><TooltipButton label="关闭设置" onClick={onClose}><Icon name="close" size={17} /></TooltipButton></div>
    <div className="settings-row"><span><Icon name={theme === 'dark' ? 'moon' : 'theme'} size={17} /><span>外观主题<small>{theme === 'dark' ? '深色 · 午夜蓝' : '浅色 · 雪白'}</small></span></span><button type="button" className="theme-switch" onClick={onToggleTheme}><span className={theme === 'light' ? 'is-light' : ''}>{theme === 'dark' ? '深' : '浅'}</span></button></div>
    <div className="settings-row"><span><Icon name="menu" size={17} /><span>界面密度<small>{displayDensity === 'compact' ? '紧凑 · 高信息密度' : viewportRequiresCompactDensity ? '窗口较小，已自动紧凑' : '舒适 · 更大间距'}</small></span></span><button type="button" aria-label="紧凑桌面布局" className={`toggle-switch ${displayDensity === 'compact' ? 'is-on' : ''}`} onClick={onToggleDensity}><span /></button></div>
    <label className="settings-row settings-select-row"><span><Icon name="clock" size={17} /><span>撤销发送<small>{undoSendSeconds === 0 ? '关闭后立即连接邮件服务器发送' : `发送后保留 ${undoSendSeconds} 秒撤销窗口`}</small></span></span><select aria-label="撤销发送等待时间" value={undoSendSeconds} onChange={(event) => { const next = Number(event.target.value); if (UNDO_SEND_OPTIONS.includes(next as UndoSendSeconds)) onUndoSendSecondsChange(next as UndoSendSeconds) }}><option value={0}>关闭</option><option value={5}>5 秒</option><option value={10}>10 秒</option><option value={20}>20 秒</option><option value={30}>30 秒</option></select></label>
    <label className="settings-row css-row"><span><Icon name="brush" size={17} /><span>用户 CSS<small>{removedUnsafeCustomCss ? '已过滤外部资源与危险语法' : '可覆盖 MailGo 视觉变量；不加载外部资源'}</small></span></span><textarea value={customCss} onChange={(event) => onCustomCssChange(event.target.value)} placeholder="例如：:root { --accent: #ff6b8a; }" /></label>
    <AccountSignatureSettings accounts={accounts} initialAccountId={selectedAccountId} onSave={onSaveSignature} />
    <div className="settings-row"><span><Icon name="shieldCheck" size={17} /><span>屏蔽规则<small>{mailRuleCount ? `${mailRuleCount} 条 · 加密保存在本机` : '按发件人或域名过滤，不上传云端'}</small></span></span><button type="button" className="settings-text-button" onClick={onOpenMailRules}>管理</button></div>
    <div className="settings-row"><span><Icon name="cloud" size={17} /><span>关闭时后台运行<small>最小化到系统托盘并继续同步</small></span></span><button type="button" className={`toggle-switch ${minimizeToTray ? 'is-on' : ''}`} onClick={onToggleMinimizeToTray}><span /></button></div>
    <div className="settings-row"><span><Icon name="image" size={17} /><span>加载远程图片<small>{remoteImagesEnabled ? '已允许 HTTPS 图片，可能包含追踪像素' : '默认屏蔽，保护隐私；CID 内嵌图片不受影响'}</small></span></span><button type="button" aria-label="加载远程图片" className={`toggle-switch ${remoteImagesEnabled ? 'is-on' : ''}`} onClick={onToggleRemoteImages}><span /></button></div>
    <div className="settings-row"><span><Icon name="bell" size={17} /><span>后台新邮件提醒<small>窗口隐藏时发送 Windows 托盘通知</small></span></span><button type="button" aria-label="后台新邮件提醒" className={`toggle-switch ${notificationsEnabled ? 'is-on' : ''}`} onClick={onToggleNotifications}><span /></button></div>
    <div className="settings-actions"><button type="button" onClick={onExportAccounts}><Icon name="download" size={16} />导出脱敏配置</button><button type="button" onClick={() => importInputRef.current?.click()} disabled={isImporting}><Icon name="folder" size={16} />{isImporting ? '导入中…' : '导入脱敏配置'}</button></div>
    <div className="settings-security-note"><Icon name="shieldCheck" size={15} /><span>导出文件不包含授权码或令牌；导入后需重新授权。</span></div>
    <input ref={importInputRef} type="file" accept="application/json,.json" hidden onChange={onImportAccounts} />
  </motion.div>
}
