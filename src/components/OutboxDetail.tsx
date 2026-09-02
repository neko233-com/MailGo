import { Icon } from './Icon'
import type { MailAccount, NativeOutboxItem } from '../types'

type OutboxDetailProps = {
  item: NativeOutboxItem
  account?: MailAccount
  busyAction?: 'edit' | 'retry' | 'discard'
  onBack: () => void
  onEdit: () => void
  onRetry: () => void
  onDiscard: () => void
}

const statusCopy = {
  scheduled: { label: '等待撤销窗口', detail: '到达发送时间后将由后台自动发送。' },
  pending: { label: '等待发送', detail: '已安全保存在本机，后台发送不会阻塞界面。' },
  retrying: { label: '自动重试中', detail: '网络恢复后会自动继续，无需停留在此页面。' },
  paused: { label: '需要处理', detail: '自动重试已暂停，请重新授权或手动重试。' },
} as const

const userScheduledCopy = {
  label: '定时发送',
  detail: '已加密保存在本机，将在计划时间由后台自动发送。',
} as const

function formatOutboxTime(timestamp: number) {
  if (!timestamp) return '—'
  const date = new Date(timestamp * 1_000)
  return Number.isNaN(date.getTime())
    ? '—'
    : date.toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })
}

function formatFileSize(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  return `${Math.max(1, Math.round(bytes / 1024))} KB`
}

function recipientRows(item: NativeOutboxItem) {
  return [
    { label: '收件人', value: item.to },
    ...(item.cc ? [{ label: '抄送', value: item.cc }] : []),
    ...(item.bcc ? [{ label: '密送', value: item.bcc }] : []),
  ]
}

export function OutboxDetail({ item, account, busyAction, onBack, onEdit, onRetry, onDiscard }: OutboxDetailProps) {
  const hasUserSchedule = Boolean(item.scheduledAt)
  const isUserScheduled = item.state === 'scheduled' && hasUserSchedule
  const status = isUserScheduled ? userScheduledCopy : statusCopy[item.state]
  const plannedAttemptAt = item.scheduledAt ?? item.nextAttemptAt
  const busy = Boolean(busyAction)
  return (
    <>
      <div className="reading-toolbar outbox-toolbar" aria-busy={busy}>
        <div className="reading-actions">
          <button type="button" className="mobile-only-button reading-back-button icon-button" aria-label="返回邮件列表" onClick={onBack}>列表</button>
          <button type="button" className="toolbar-action" onClick={onEdit} disabled={busy}><Icon name="edit" size={17} /><span>{busyAction === 'edit' ? '正在打开…' : '编辑并继续'}</span></button>
          <button type="button" className="toolbar-action" onClick={onRetry} disabled={busy}><Icon name="rotate" size={17} /><span>{busyAction === 'retry' ? '正在发送…' : hasUserSchedule ? '立即发送' : '立即重试'}</span></button>
          <button type="button" className="toolbar-action is-danger" onClick={onDiscard} disabled={busy}><Icon name="trash" size={17} /><span>删除待发送</span></button>
        </div>
      </div>
      <div className="reading-scroll outbox-reading-scroll">
        <div className="reading-heading outbox-heading">
          <div>
            <h1>{item.subject || '(无主题)'}</h1>
            <div className="message-tags">
              <span className={`tag outbox-status is-${item.state}`}><Icon name={item.state === 'paused' ? 'info' : item.state === 'scheduled' ? 'clock' : 'rotate'} size={13} />{status.label}</span>
              <span className="tag tag-account">{account?.label ?? '当前账户'}</span>
            </div>
          </div>
        </div>

        <section className="outbox-summary" aria-label="待发送状态">
          <div className={`outbox-state-mark is-${item.state}`}><Icon name={item.state === 'paused' ? 'info' : item.state === 'scheduled' ? 'clock' : 'send'} size={19} /></div>
          <div><strong>{status.label}</strong><p>{item.lastError || status.detail}</p></div>
          <dl>
            <div><dt>加入发件箱</dt><dd>{formatOutboxTime(item.createdAt)}</dd></div>
            {plannedAttemptAt > 0 && <div><dt>{hasUserSchedule ? '计划发送' : '下次尝试'}</dt><dd>{formatOutboxTime(plannedAttemptAt)}</dd></div>}
            <div><dt>尝试次数</dt><dd>{item.attempts} 次</dd></div>
          </dl>
        </section>

        <section className="outbox-recipient-card" aria-label="收件信息">
          <div className="outbox-account-line"><span className="outbox-account-avatar" style={{ background: account?.accent }}>{account?.label.slice(0, 1) ?? 'M'}</span><span><strong>{account?.label ?? 'MailGo 账户'}</strong><small>{account?.email ?? item.accountId}</small></span></div>
          <dl>{recipientRows(item).map((row) => <div key={row.label}><dt>{row.label}</dt><dd><bdi dir="auto">{row.value}</bdi></dd></div>)}</dl>
        </section>

        <section className="outbox-preview-card" aria-label="邮件摘要">
          <span>正文摘要</span>
          <p>{item.preview || '无纯文本摘要'}</p>
          <small>为保证发件箱秒开，这里只加载本地摘要；选择“编辑并继续”可打开完整草稿。</small>
        </section>

        {item.attachments.length > 0 && <section className="outbox-attachments" aria-label="待发送附件">
          <h2><Icon name="paperclip" size={17} />{item.attachments.length} 个附件</h2>
          <div>{item.attachments.map((attachment, index) => <span key={`${attachment.fileName}:${index}`}><Icon name={attachment.inline ? 'image' : 'document'} size={16} /><bdi dir="auto">{attachment.fileName}</bdi><small>{formatFileSize(attachment.size)}{attachment.inline ? ' · 内嵌图片' : ''}</small></span>)}</div>
        </section>}
      </div>
    </>
  )
}
