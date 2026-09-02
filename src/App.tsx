import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { startTransition, useCallback, useDeferredValue, useEffect, useMemo, useRef, useState } from 'react'
import appIconUrl from '../resources/icons/mailgo-64.png'
import { AccountSignatureSettings } from './components/AccountSignatureSettings'
import { ConfirmDialog } from './components/ConfirmDialog'
import { ExternalLinkDialog } from './components/ExternalLinkDialog'
import { Icon, type IconName } from './components/Icon'
import { MailRuleManager } from './components/MailRuleManager'
import { OutboxDetail } from './components/OutboxDetail'
import { RecipientInput } from './components/RecipientInput'
import { RichTextEditor } from './components/RichTextEditor'
import { ScheduleSendControl } from './components/ScheduleSendControl'
import { SnoozeControl } from './components/SnoozeControl'
import { buildComposeThreadHeaders, type ComposeMode } from './compose-thread'
import { sanitizeCustomCss } from './customCss'
import { folderLabels, providerDefinitions, sampleAccounts, sampleMails, sampleOutboxItems } from './data'
import { mapWithConcurrency } from './lib/asyncPool'
import { invoke, readNativeState } from './lib/ipc'
import { inspectExternalLink, type ExternalLinkInspection } from './linkSafety'
import { applyMailRules, domainFromSender } from './mailRules'
import { mailNeedsBodyHydration, selectBodyHydrationCandidates } from './messageHydration'
import { appendSignatureToComposeHtml, plainTextToComposeHtml, sanitizeComposeHtml } from './richText'
import { appendAccountSignature, normalizeAccountSignature } from './signature'
import { formatScheduledAt } from './scheduleSend'
import { formatSnoozeTime } from './snooze'
import { buildMailThreads, type MailThread } from './threading'
import { sanitizeHtml } from './htmlSafety'
import type { FolderId, MailAccount, MailAttachment, MailMessage, MailRuleKind, NativeAttachmentChunkResponse, NativeAttachmentStartResponse, NativeAttachmentUploadChunkResponse, NativeAttachmentUploadStartResponse, NativeAuthStartResponse, NativeCacheStats, NativeCacheStatsResponse, NativeCachedMessage, NativeConnectionDiagnostic, NativeConnectionDiagnosticChannel, NativeDeviceStartResponse, NativeDraft, NativeDraftAttachment, NativeLocalSearchResponse, NativeMailboxResponse, NativeMailRule, NativeMailRuleSnapshot, NativeMessageResponse, NativeOutboxActionResponse, NativeOutboxItem, NativeOutboxRecallResponse, NativeOutboxSnapshot, NativeQueueStatus, NativeSearchResponse, NativeSendResponse, NativeSnoozeSnapshot, NativeSyncItem, NativeSyncResponse, NativeUndoSendResponse, Provider, SmartCategory, ThemeMode } from './types'

type ToastTone = 'info' | 'success' | 'error'
type UndoSendSeconds = 0 | 5 | 10 | 20 | 30
type ToastAction = { kind: 'undo-send'; accountId: string; outboxId: string; draftId?: string }
type Toast = { id: number; message: string; tone: ToastTone; durationMs: number; action?: ToastAction }
type ToastOptions = { action?: ToastAction; durationMs?: number; onExpire?: () => void }
type DeviceFlowState = { sessionId: string; userCode: string; verificationUri: string; message?: string; retryAfter: number; status: 'pending' | 'complete' | 'error' }
type ConnectionDiagnosticViewState =
  | { phase: 'checking' }
  | { phase: 'ready'; result: NativeConnectionDiagnostic }
  | { phase: 'error'; message: string }
type ActionMenu = 'bulk' | 'message'
type MobilePane = 'list' | 'reading'
type DisplayDensity = 'compact' | 'comfortable'
type MailContentScale = 70 | 80 | 90 | 100
type MailMoveTarget = { folder: string; label: string; icon: IconName }
type OutboxAction = { id: string; kind: 'edit' | 'retry' | 'discard' }
type VirtualMailItem =
  | { type: 'group'; key: string; label: string }
  | { type: 'thread'; key: string; thread: MailThread }
const MAX_CUSTOM_CSS_LENGTH = 64 * 1024
const INITIAL_MAILBOX_PAGE_SIZE = 48
const EARLIER_MAILBOX_PAGE_SIZE = 50
const ACCOUNT_IPC_CONCURRENCY = 4
const BULK_ACTION_IPC_CONCURRENCY = 6
const ATTACHMENT_DOWNLOAD_CONCURRENCY = 3
const DEFAULT_UNDO_SEND_SECONDS: UndoSendSeconds = 10
const MAIL_CONTENT_SCALES: MailContentScale[] = [70, 80, 90, 100]
const DEFAULT_MAIL_CONTENT_SCALE: MailContentScale = 80
const MOBILE_LAYOUT_QUERY = '(max-width: 720px)'
const COMPACT_DENSITY_QUERY = '(max-height: 820px), (max-width: 1366px)'
const AUTO_COLLAPSE_SIDEBAR_QUERY = '(max-width: 1366px) and (min-width: 721px)'
const COMPACT_MAIL_GROUP_HEIGHT = 16
const COMPACT_MAIL_ROW_HEIGHT = 36
const COMFORTABLE_MAIL_GROUP_HEIGHT = 22
const COMFORTABLE_MAIL_ROW_HEIGHT = 48
const MOBILE_MAIL_GROUP_HEIGHT = 18
const MOBILE_MAIL_ROW_HEIGHT = 44
const SNOOZE_TIMER_RECHECK_MS = 5 * 60 * 1_000
const DANGEROUS_ATTACHMENT_EXTENSIONS = new Set([
  'ade', 'adp', 'app', 'application', 'appinstaller', 'appref-ms', 'appx', 'appxbundle', 'bat',
  'bin', 'cab', 'chm', 'cmd', 'com', 'command', 'cpl', 'desktop', 'dll', 'dmg', 'docm', 'drv',
  'exe', 'gadget', 'hta', 'inf', 'ins', 'iso', 'isp', 'jar', 'js', 'jse', 'lnk', 'mde', 'msc',
  'msi', 'msix', 'msp', 'mst', 'msu', 'ocx', 'one', 'pif', 'pkg', 'potm', 'ppam', 'ppsm', 'pptm',
  'ps1', 'ps1xml', 'ps2', 'ps2xml', 'psc1', 'psc2', 'reg', 'scf', 'scr', 'sct', 'sh', 'shb',
  'sys', 'url', 'vb', 'vbe', 'vbs', 'vhd', 'vhdx', 'vxd', 'website', 'ws', 'wsc', 'wsf', 'wsh',
  'xlam', 'xll', 'xlsm',
])
const IMAGE_ATTACHMENT_EXTENSIONS = new Set(['bmp', 'gif', 'jpeg', 'jpg', 'png', 'tif', 'tiff', 'webp'])
const SHEET_ATTACHMENT_EXTENSIONS = new Set(['csv', 'ods', 'xls', 'xlsb', 'xlsm', 'xlsx'])

type MailboxPagingMeta = {
  oldestUid?: number
  hasMore: boolean
  localHasMore: boolean
  remoteHasMore: boolean
  totalCached: number
  revision: number
}

const smartCategories: { id: SmartCategory; label: string; icon: IconName; color: string }[] = [
  { id: 'apple-connect', label: 'Apple Connect 通知', icon: 'bell', color: '#8b95aa' },
  { id: 'apple-ads', label: 'Apple 广告', icon: 'grid', color: '#ed7191' },
  { id: 'social', label: '社交通知', icon: 'message', color: '#46cfa1' },
  { id: 'ads', label: '其他广告', icon: 'bell', color: '#f0a868' },
]

const inboxTabs: { id: string; label: string; icon: IconName; category: SmartCategory | null }[] = [
  { id: 'primary', label: '主要', icon: 'inbox', category: null },
  { id: 'updates', label: '动态', icon: 'bell', category: 'social' },
  { id: 'apple-connect', label: 'Apple 通知', icon: 'bell', category: 'apple-connect' },
  { id: 'promotions', label: '推广', icon: 'grid', category: 'ads' },
]

const initialHtml = `
  <article class="safe-html-card">
    <div class="safe-html-brand">APPLE ACCOUNT</div>
    <h3>登录提醒</h3>
    <p>检测到你的 Apple Account 在新设备上登录。如果这是你本人操作，则无需采取任何措施。</p>
    <a href="https://account.apple.com" target="_blank" rel="noreferrer">检查账户活动</a>
  </article>
`

function providerFor(provider: Provider) {
  return providerDefinitions.find((item) => item.id === provider) ?? providerDefinitions[3]
}

function isSupportedProvider(value: unknown): value is Provider {
  return value === 'google' || value === 'qq' || value === 'outlook' || value === 'other'
}

function asUndoSendSeconds(value: unknown): UndoSendSeconds {
  return value === 0 || value === 5 || value === 10 || value === 20 || value === 30
    ? value
    : DEFAULT_UNDO_SEND_SECONDS
}

function connectionDiagnosticError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error)
  if (/already in progress/i.test(message)) return '该账户正在同步，请稍后再检测'
  if (/offline-only|offline mode/i.test(message)) return '请先关闭仅离线模式'
  if (/auth|credential|login|password|authorization/i.test(message)) return '当前凭据不可用，请重新授权后再检测'
  return '暂时无法完成连接检测，请检查网络后重试'
}

const diagnosticStatusLabels = {
  ok: '连接正常',
  authentication: '需要重新授权',
  'rate-limit': '服务商暂时限流',
  network: '网络不可达',
  tls: '安全连接失败',
  provider: '服务商拒绝连接',
} as const

function formatCount(value: number) {
  return value > 99 ? '99+' : String(value)
}

function escapeHtml(input: string) {
  return input
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

function bytesToBase64(bytes: Uint8Array) {
  let binary = ''
  const blockSize = 0x8000
  for (let offset = 0; offset < bytes.length; offset += blockSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + blockSize))
  }
  return btoa(binary)
}

function nativeCategory(category: NativeCachedMessage['category']): SmartCategory | undefined {
  return category === 'inbox' ? undefined : category
}

function uiFolderForNative(folder: string): FolderId {
  const normalized = folder.toLowerCase()
  if (normalized === 'inbox') return 'inbox'
  if (normalized.includes('sent')) return 'sent'
  if (normalized.includes('draft')) return 'drafts'
  if (normalized.includes('spam') || normalized.includes('junk')) return 'spam'
  if (normalized.includes('trash') || normalized.includes('deleted')) return 'trash'
  return 'archive'
}

function nativeFolderName(account: MailAccount, folder: FolderId): string {
  if (folder === 'outbox') return '__MAILGO_LOCAL_OUTBOX__'
  if (folder === 'snoozed') return '__MAILGO_LOCAL_SNOOZED__'
  if (folder === 'inbox') return 'INBOX'
  if (folder === 'sent') return account.provider === 'google' ? '[Gmail]/Sent Mail' : account.provider === 'outlook' ? 'Sent Items' : 'Sent Messages'
  if (folder === 'drafts') return account.provider === 'google' ? '[Gmail]/Drafts' : 'Drafts'
  if (folder === 'spam') return account.provider === 'google' ? '[Gmail]/Spam' : account.provider === 'outlook' ? 'Junk Email' : 'Spam'
  if (folder === 'trash') return account.provider === 'google' ? '[Gmail]/Trash' : account.provider === 'outlook' ? 'Deleted Items' : 'Trash'
  return account.provider === 'google' ? '[Gmail]/All Mail' : 'Archive'
}

function nativeFolderLabel(folder: string, displayName?: string) {
  const source = displayName?.trim() || folder
  const parts = source.split(/[\\/]/).filter(Boolean)
  return parts[parts.length - 1] || source
}

function isSameNativeFolder(left: string, right: string) {
  return left.toLocaleLowerCase() === right.toLocaleLowerCase()
}

function customNativeFolders(account: MailAccount, folders: string[] | undefined) {
  const builtInFolders = folderLabels
    .filter((folder) => folder.id !== 'starred' && folder.id !== 'outbox')
    .map((folder) => nativeFolderName(account, folder.id))
  const result: string[] = []
  for (const folder of folders ?? []) {
    if (!folder.trim() || folder.length > 512 || /[\\r\\n]/.test(folder) || builtInFolders.some((builtIn) => isSameNativeFolder(builtIn, folder))) continue
    if (!result.some((known) => isSameNativeFolder(known, folder))) result.push(folder)
    if (result.length === 64) break
  }
  return result
}

function nativeMoveTargets(account: MailAccount, folders: string[] | undefined, labels?: Record<string, string>): MailMoveTarget[] {
  const fixedTargets: Array<{ id: Extract<FolderId, 'inbox' | 'archive' | 'spam' | 'trash'>; label: string; icon: IconName }> = [
    { id: 'inbox', label: '收件箱', icon: 'inbox' },
    { id: 'archive', label: '归档', icon: 'archive' },
    { id: 'spam', label: '垃圾邮件', icon: 'shield' },
    { id: 'trash', label: '回收站', icon: 'trash' },
  ]
  const targets = fixedTargets.map(({ id, label, icon }) => ({
    folder: nativeFolderName(account, id),
    label,
    icon,
  }))
  return targets.concat(customNativeFolders(account, folders).map((folder) => ({
    folder,
    label: nativeFolderLabel(folder, labels?.[folder]),
    icon: 'folder' as const,
  })))
}

function nativeMailboxKey(accountId: string, folder: string) {
  return `${accountId}::${folder}`
}

function nativeFolderCountKey(accountId: string, folder: string) {
  return `${accountId}\u0000${folder.toLocaleLowerCase()}`
}

function sameStringArrayRecord(left: Record<string, string[]>, right: Record<string, string[]>) {
  const leftKeys = Object.keys(left)
  const rightKeys = Object.keys(right)
  if (leftKeys.length !== rightKeys.length) return false
  return leftKeys.every((key) => {
    const leftValues = left[key]
    const rightValues = right[key]
    return Boolean(rightValues)
      && leftValues.length === rightValues.length
      && leftValues.every((value, index) => value === rightValues[index])
  })
}

function sameNestedStringRecord(left: Record<string, Record<string, string>>, right: Record<string, Record<string, string>>) {
  const leftKeys = Object.keys(left)
  const rightKeys = Object.keys(right)
  if (leftKeys.length !== rightKeys.length) return false
  return leftKeys.every((key) => {
    const leftValues = left[key]
    const rightValues = right[key]
    if (!rightValues) return false
    const valueKeys = Object.keys(leftValues)
    return valueKeys.length === Object.keys(rightValues).length
      && valueKeys.every((valueKey) => leftValues[valueKey] === rightValues[valueKey])
  })
}

function attachmentExtension(fileName: string) {
  const match = /\.([a-z0-9]{1,12})$/i.exec(fileName.trim())
  return match?.[1].toLocaleLowerCase('en-US') ?? ''
}

function isDangerousAttachmentName(fileName: string) {
  return DANGEROUS_ATTACHMENT_EXTENSIONS.has(attachmentExtension(fileName))
}

function nativeAttachmentKind(contentType: string, fileName: string): MailAttachment['kind'] {
  const extension = attachmentExtension(fileName)
  if (contentType === 'application/pdf' && extension === 'pdf') return 'pdf'
  if ((contentType.includes('spreadsheet') || contentType.includes('excel')) && SHEET_ATTACHMENT_EXTENSIONS.has(extension)) return 'sheet'
  if (contentType.startsWith('image/') && IMAGE_ATTACHMENT_EXTENSIONS.has(extension)) return 'image'
  return 'file'
}

type AttachmentCardProps = {
  attachment: MailAttachment
  progress?: number
  onActivate: () => void
}

function AttachmentCard({ attachment, progress, onActivate }: AttachmentCardProps) {
  const extension = attachmentExtension(attachment.name)
  const dangerous = isDangerousAttachmentName(attachment.name)
  const detail = progress != null
    ? `${progress}% · 点击取消`
    : `${attachment.size} · ${extension ? `.${extension}` : '无扩展名'}${dangerous ? ' · 谨慎打开' : ''}`
  const glyph = attachment.kind === 'pdf' ? 'PDF' : attachment.kind === 'sheet' ? 'X' : attachment.kind === 'image' ? 'IMG' : 'FILE'

  return (
    <button
      type="button"
      className={`attachment-card${dangerous ? ' is-dangerous' : ''}`}
      onClick={onActivate}
      title={dangerous ? `高风险附件类型：${extension ? `.${extension}` : '未知扩展名'}` : attachment.name}
    >
      <span className={`file-glyph file-${attachment.kind}`}>{glyph}</span>
      <span className="attachment-copy">
        <strong><bdi dir="auto">{attachment.name}</bdi></strong>
        <small>{detail}</small>
      </span>
      {dangerous ? <Icon name="shield" size={16} className="attachment-risk-icon" /> : null}
      <Icon name={progress != null ? 'close' : 'download'} size={17} />
    </button>
  )
}

function formatStorageBytes(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const unitIndex = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const value = bytes / 1024 ** unitIndex
  return `${value >= 100 || unitIndex === 0 ? value.toFixed(0) : value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${units[unitIndex]}`
}

function storageShare(bytes: number, total: number) {
  return total > 0 ? `${Math.max(0, Math.min(100, (bytes / total) * 100))}%` : '0%'
}

function plainTextFromHtml(input: string | undefined) {
  if (!input) return ''
  const documentParser = new DOMParser().parseFromString(sanitizeHtml(input), 'text/html')
  return (documentParser.body.textContent ?? '').replace(/\s+/g, ' ').trim().slice(0, 200_000)
}

function nativeMessageToUi(message: NativeCachedMessage, account: MailAccount): MailMessage {
  const date = message.receivedAt ? new Date(message.receivedAt) : null
  const validDate = date && !Number.isNaN(date.getTime()) ? date : null
  const timestamp = validDate
    ? validDate.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })
    : '—'
  const dateGroup = validDate
    ? validDate.toLocaleDateString('zh-CN', { month: 'numeric', day: 'numeric' })
    : '最近'
  const senderName = message.senderName || message.senderEmail || '未知发件人'
  const htmlPlainText = message.textBody ? '' : plainTextFromHtml(message.htmlBody)
  return {
    id: `${message.accountId}:${message.folder}:${message.uid}`,
    messageId: message.messageId,
    inReplyTo: message.inReplyTo,
    references: message.references,
    threadId: message.threadId,
    accountId: message.accountId,
    folder: uiFolderForNative(message.folder),
    category: nativeCategory(message.category),
    from: message.senderEmail || 'unknown@example.com',
    senderName,
    subject: message.subject || '(无主题)',
    to: message.to,
    cc: message.cc,
    preview: message.preview || message.textBody.slice(0, 240),
    timestamp,
    receivedAt: message.receivedAt,
    dateGroup,
    unread: message.unread,
    starred: message.starred,
    isAd: message.isAd,
    blocked: message.blocked,
    blockedRuleId: message.blockedRuleId,
    accent: account.accent,
    avatar: senderName.split(/\s+/).map((part) => part[0]).join('').slice(0, 2).toUpperCase() || '?',
    body: message.textBody
      ? message.textBody.split(/\r?\n\s*\r?\n/).filter(Boolean)
      : htmlPlainText
        ? [htmlPlainText]
        : ['正在加载邮件正文…'],
    attachments: message.attachments.map((attachment, index) => ({
      id: `${message.accountId}:${message.uid}:attachment:${index}`,
      name: attachment.fileName || 'attachment',
      size: attachment.size > 1024 * 1024 ? `${(attachment.size / 1024 / 1024).toFixed(1)} MB` : `${Math.max(1, Math.round(attachment.size / 1024))} KB`,
      kind: nativeAttachmentKind(attachment.contentType, attachment.fileName),
      nativeIndex: attachment.index,
    })),
    hasHtml: Boolean(message.htmlBody),
    htmlBody: message.htmlBody,
    nativeUid: message.uid,
    nativeFolder: message.folder,
  }
}

function snoozeSnapshotToUi(snapshot: NativeSnoozeSnapshot, accounts: MailAccount[]) {
  const accountDirectory = new Map(accounts.map((account) => [account.id, account]))
  return snapshot.items.flatMap((item) => {
    const account = accountDirectory.get(item.message.accountId)
    return account ? [{ ...nativeMessageToUi(item.message, account), snoozedUntil: item.wakeAt }] : []
  })
}

function pagingMetaFromResponse(result: NativeMailboxResponse): MailboxPagingMeta | undefined {
  if (!result.mailbox) return undefined
  const remoteHasMore = result.remoteHasMore ?? Boolean(result.mailbox.hasMore)
  const localHasMore = result.localHasMore ?? false
  return {
    oldestUid: result.mailbox.oldestUid,
    hasMore: localHasMore || remoteHasMore,
    localHasMore,
    remoteHasMore,
    totalCached: Math.max(0, result.totalCached ?? result.mailbox.messages.length),
    revision: Math.max(0, result.revision ?? 0),
  }
}

function mergeMailboxPage(current: MailMessage[], incoming: MailMessage[], accountId: string, folder: string, mode: 'replace' | 'latest' | 'append') {
  if (mode === 'latest' && incoming.length === 0) return current
  const incomingIds = new Set(incoming.map((mail) => mail.id))
  const oldestIncomingUid = incoming.reduce<number | undefined>((oldest, mail) => (
    mail.nativeUid == null ? oldest : oldest == null ? mail.nativeUid : Math.min(oldest, mail.nativeUid)
  ), undefined)
  const retained = current.filter((mail) => {
    const sameFolder = mail.accountId === accountId && Boolean(mail.nativeFolder) && isSameNativeFolder(mail.nativeFolder!, folder)
    if (!sameFolder) return true
    if (mode === 'replace') return false
    if (incomingIds.has(mail.id)) return false
    if (mode === 'latest' && oldestIncomingUid != null && mail.nativeUid != null) return mail.nativeUid < oldestIncomingUid
    return true
  })
  return [...retained, ...incoming]
}

function draftToUi(draft: NativeDraft, account: MailAccount): MailMessage {
  const date = new Date(draft.updatedAt * 1000)
  const validDate = !Number.isNaN(date.getTime()) ? date : null
  return {
    id: `local-draft:${draft.accountId}:${draft.id}`,
    accountId: draft.accountId,
    folder: 'drafts',
    from: account.email,
    senderName: `${account.label} · 草稿`,
    subject: draft.subject || '(无主题)',
    preview: draft.body.trim() || '未填写邮件内容',
    timestamp: validDate ? validDate.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' }) : '—',
    dateGroup: '本地草稿',
    unread: false,
    starred: false,
    accent: account.accent,
    avatar: '草',
    body: draft.body ? draft.body.split(/\r?\n\s*\r?\n/).filter(Boolean) : ['这是一封尚未完成的本地草稿。'],
  }
}

function outboxToUi(item: NativeOutboxItem, account: MailAccount): MailMessage {
  const date = new Date((item.scheduledAt ?? item.updatedAt) * 1_000)
  const validDate = !Number.isNaN(date.getTime()) ? date : null
  const recipient = item.to.split(/[,;]/).map((value) => value.trim()).find(Boolean) ?? '未填写收件人'
  return {
    id: `local-outbox:${item.accountId}:${item.id}`,
    messageId: `outbox:${item.id}`,
    threadId: `outbox:${item.id}`,
    accountId: item.accountId,
    folder: 'outbox',
    from: account.email,
    senderName: `发送给 ${recipient}`,
    subject: item.subject || '(无主题)',
    to: item.to.split(/[,;]/).map((value) => value.trim()).filter(Boolean),
    cc: item.cc.split(/[,;]/).map((value) => value.trim()).filter(Boolean),
    preview: item.lastError || item.preview || '等待后台发送',
    timestamp: validDate ? validDate.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' }) : '—',
    receivedAt: validDate?.toISOString(),
    dateGroup: item.scheduledAt ? '定时发送' : '待发送',
    unread: false,
    starred: false,
    accent: account.accent,
    avatar: item.state === 'paused' ? '!' : item.scheduledAt ? '时' : '发',
    body: [item.preview || '无纯文本摘要'],
    outboxId: item.id,
    outboxState: item.state,
    outboxScheduledAt: item.scheduledAt,
  }
}

type ComposeAttachmentStatus = 'saving' | 'saved' | 'failed' | 'removing'
type ComposeAttachmentItem = {
  localId: string
  persistedId?: string
  fileName: string
  contentType: string
  size: number
  file?: File
  status: ComposeAttachmentStatus
}
type ComposeInlineImage = ComposeAttachmentItem & { contentId: string; previewUrl?: string }

function composeSubject(mode: ComposeMode, subject: string) {
  const trimmed = subject.trim() || '(无主题)'
  if (mode === 'reply' || mode === 'reply-all') return /^(re\s*:\s*)/i.test(trimmed) ? trimmed : `Re: ${trimmed}`
  if (mode === 'forward') return /^(fwd\s*:\s*)/i.test(trimmed) ? trimmed : `Fwd: ${trimmed}`
  return trimmed
}

function uniqueRecipients(values: string[], ownEmail?: string) {
  const seen = new Set<string>()
  return values.filter((value) => {
    const normalized = value.trim().toLowerCase()
    if (!normalized || normalized === ownEmail?.trim().toLowerCase() || seen.has(normalized)) return false
    seen.add(normalized)
    return true
  })
}

function quoteMail(source: MailMessage, mode: ComposeMode) {
  const body = source.body.join('\n\n').slice(0, 20_000)
  if (mode === 'forward') {
    return `\n\n---------- 转发邮件 ----------\n发件人：${source.senderName} <${source.from}>\n主题：${source.subject}\n日期：${source.timestamp}\n\n${body}`
  }
  return `\n\n---------- 原始邮件 ----------\n${source.senderName} <${source.from}> 在 ${source.timestamp} 写道：\n${body.split(/\r?\n/).map((line) => `> ${line}`).join('\n')}`
}

function composeSeed(mode: ComposeMode, source: MailMessage | undefined, ownEmail?: string) {
  if (!source || mode === 'new') return { to: '', cc: '', subject: '', body: '' }
  const replyAllCc = uniqueRecipients([...(source.to ?? []), ...(source.cc ?? [])], ownEmail)
    .filter((recipient) => recipient.toLowerCase() !== source.from.trim().toLowerCase())
  return {
    to: source.from,
    cc: mode === 'reply-all' ? replyAllCc.join(', ') : '',
    subject: composeSubject(mode, source.subject),
    body: quoteMail(source, mode),
  }
}

function createInlineContentId() {
  const randomPart = typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`
  return `mailgo-inline-${randomPart.replace(/[^a-zA-Z0-9-]/g, '')}`
}

function createComposeAttachmentId() {
  const randomPart = typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`
  return `compose-attachment-${randomPart.replace(/[^a-zA-Z0-9-]/g, '')}`
}

function createAccountId(provider: Provider) {
  const randomPart = typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function'
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`
  return `${provider}-${randomPart.replace(/[^a-zA-Z0-9-]/g, '')}`
}

function sameMailboxIdentity(left: MailAccount, right: MailAccount) {
  const normalize = (value: string | undefined) => value?.trim().toLowerCase() ?? ''
  if (left.provider !== right.provider || normalize(left.email) !== normalize(right.email)) return false
  if (left.provider !== 'other') return true
  return normalize(left.imapHost) === normalize(right.imapHost)
    && left.imapPort === right.imapPort
    && normalize(left.imapSecurity) === normalize(right.imapSecurity)
    && normalize(left.smtpHost) === normalize(right.smtpHost)
    && left.smtpPort === right.smtpPort
    && normalize(left.smtpSecurity) === normalize(right.smtpSecurity)
}

function composeHtmlBody(body: string, inlineImages: ComposeInlineImage[], richBody?: string, signature = '') {
  const textBlock = richBody?.trim()
    ? sanitizeComposeHtml(richBody)
    : plainTextToComposeHtml(body)
  const signedBody = appendSignatureToComposeHtml(textBlock, signature)
  const imageBlocks = inlineImages.map(({ fileName, contentId }) => (
    `<p><img src="cid:${escapeHtml(contentId)}" alt="${escapeHtml(fileName)}"></p>`
  )).join('')
  return sanitizeComposeHtml(signedBody + imageBlocks)
}

function readLocalStorageValue(key: string) {
  try {
    return window.localStorage.getItem(key)
  } catch {
    return null
  }
}

function writeLocalStorageValue(key: string, value: string) {
  try {
    window.localStorage.setItem(key, value)
  } catch {
    // Native state or the in-memory session remains authoritative when storage is unavailable.
  }
}

function loadTheme(): ThemeMode {
  const stored = readLocalStorageValue('mailgo-theme')
  return stored === 'dark' ? 'dark' : 'light'
}

function loadDisplayDensity(): DisplayDensity {
  return readLocalStorageValue('mailgo-display-density-v3') === 'comfortable' ? 'comfortable' : 'compact'
}

function loadMailContentScale(): MailContentScale {
  const stored = Number(readLocalStorageValue('mailgo-mail-content-scale-v2'))
  return MAIL_CONTENT_SCALES.includes(stored as MailContentScale)
    ? stored as MailContentScale
    : DEFAULT_MAIL_CONTENT_SCALE
}

function loadRemoteImages() {
  return readLocalStorageValue('mailgo-remote-images') === 'true'
}

function loadHideAds() {
  return readLocalStorageValue('mailgo-hide-ads') === 'true'
}

function loadOfflineMode() {
  return readLocalStorageValue('mailgo-offline-mode') === 'true'
}

function loadCustomCss() {
  const stored = (readLocalStorageValue('mailgo-custom-css') ?? '').slice(0, MAX_CUSTOM_CSS_LENGTH)
  return sanitizeCustomCss(stored).css
}

function ProviderMark({ provider, size = 'md' }: { provider: Provider; size?: 'sm' | 'md' | 'lg' }) {
  const definition = providerFor(provider)
  return (
    <span className={`provider-mark provider-${provider} provider-mark-${size}`} style={{ '--provider-accent': definition.accent } as React.CSSProperties}>
      {provider === 'google' ? <span className="google-mark">G</span> : provider === 'outlook' ? <span className="outlook-mark">O</span> : provider === 'qq' ? <span className="qq-mark">Q</span> : '@'}
    </span>
  )
}

function BrandMark() {
  return (
    <span className="brand-mark" aria-hidden="true">
      <img src={appIconUrl} alt="" />
    </span>
  )
}

function TooltipButton({ label, onClick, children, active = false, className = '', ariaExpanded, disabled = false }: { label: string; onClick?: () => void; children: React.ReactNode; active?: boolean; className?: string; ariaExpanded?: boolean; disabled?: boolean }) {
  return (
    <button className={`icon-button ${active ? 'is-active' : ''} ${className}`} onClick={onClick} aria-label={label} aria-expanded={ariaExpanded} title={label} type="button" disabled={disabled}>
      {children}
    </button>
  )
}

function OfflineModeQuickSetting({ enabled, onToggle }: { enabled: boolean; onToggle: () => void }) {
  return (
    <button type="button" className={enabled ? 'is-on' : ''} aria-label={enabled ? '仅离线模式已开启' : '开启仅离线模式'} aria-pressed={enabled} onClick={onToggle}>
      <Icon name={enabled ? 'cloud' : 'rotate'} size={15} />
      <span>仅离线模式</span>
      <small>{enabled ? '暂停联网' : '在线同步'}</small>
    </button>
  )
}

function Avatar({ message, size = 'md' }: { message: MailMessage; size?: 'sm' | 'md' | 'lg' }) {
  return <span className={`avatar avatar-${size}`} style={{ '--avatar-accent': message.accent } as React.CSSProperties}>{message.avatar}</span>
}

function ConversationStack({ thread, selectedId, loadingId, onSelect }: { thread?: MailThread; selectedId: string; loadingId: string | null; onSelect: (mail: MailMessage) => void }) {
  if (!thread || thread.messages.length <= 1) return null
  return (
    <section className="conversation-stack" aria-label={`会话中的 ${thread.messages.length} 封邮件`}>
      <div className="conversation-stack-heading"><span><Icon name="message" size={15} />会话</span><strong>{thread.messages.length} 封</strong></div>
      <div className="conversation-stack-items">
        {thread.messages.map((mail, index) => {
          const selected = mail.id === selectedId
          return <button type="button" key={mail.id} className={`conversation-message ${selected ? 'is-current' : ''} ${mail.unread ? 'is-unread' : ''}`} aria-current={selected ? 'true' : undefined} onClick={() => onSelect(mail)}><Avatar message={mail} size="sm" /><span className="conversation-message-copy"><span><strong>{mail.senderName}</strong><em>{index + 1} / {thread.messages.length}</em><time>{mail.timestamp}</time></span><small>{loadingId === mail.id ? '正在补全正文…' : mail.preview || mail.subject}</small></span><Icon name={selected ? 'checkCircle' : 'forward'} size={16} /></button>
        })}
      </div>
    </section>
  )
}

function ToastView({ toast, onAction }: { toast: Toast; onAction: (toast: Toast) => void }) {
  return (
    <motion.div
      className={`toast toast-${toast.tone} ${toast.action ? 'toast-actionable' : ''}`}
      initial={{ opacity: 0, y: 12, scale: 0.98 }}
      animate={{ opacity: 1, y: 0, scale: 1 }}
      exit={{ opacity: 0, y: 12 }}
      style={{ '--toast-duration': `${toast.durationMs}ms` } as React.CSSProperties}
      role="status"
    >
      <Icon name={toast.tone === 'success' ? 'checkCircle' : toast.tone === 'error' ? 'info' : 'bell'} size={17} />
      <span>{toast.message}</span>
      {toast.action && <button type="button" onClick={() => onAction(toast)}>撤销发送</button>}
      {toast.action && <span className="toast-progress" aria-hidden="true" />}
    </motion.div>
  )
}

function App() {
  const isNativeRuntime = Boolean(window.ipc?.postMessage)
  const prefersReducedMotion = useReducedMotion()
  const [theme, setTheme] = useState<ThemeMode>(loadTheme)
  const [accounts, setAccounts] = useState<MailAccount[]>(() => isNativeRuntime ? [] : sampleAccounts)
  const [mails, setMails] = useState<MailMessage[]>(() => isNativeRuntime ? [] : sampleMails.map((mail) => ({ ...mail, body: [...mail.body], attachments: mail.attachments?.map((attachment) => ({ ...attachment })) })))
  const [localSearchMails, setLocalSearchMails] = useState<MailMessage[]>([])
  const [localSearchState, setLocalSearchState] = useState<'idle' | 'searching' | 'indexing' | 'ready' | 'error'>('idle')
  const [localSearchTruncated, setLocalSearchTruncated] = useState(false)
  const [serverSearchMails, setServerSearchMails] = useState<MailMessage[]>([])
  const [serverSearchState, setServerSearchState] = useState<'idle' | 'searching' | 'ready' | 'error'>('idle')
  const [serverSearchTruncated, setServerSearchTruncated] = useState(false)
  const [selectedFolder, setSelectedFolder] = useState<FolderId>('inbox')
  const [selectedCategory, setSelectedCategory] = useState<SmartCategory | null>(null)
  const [selectedAccountId, setSelectedAccountId] = useState<string | null>(null)
  const [selectedMailId, setSelectedMailId] = useState(() => isNativeRuntime ? '' : 'launch-plan')
  const [selectedMailIds, setSelectedMailIds] = useState<string[]>([])
  const [mobilePane, setMobilePane] = useState<MobilePane>('list')
  const [isMobileLayout, setMobileLayout] = useState(() => window.matchMedia(MOBILE_LAYOUT_QUERY).matches)
  const [displayDensity, setDisplayDensity] = useState<DisplayDensity>(loadDisplayDensity)
  const [viewportRequiresCompactDensity, setViewportRequiresCompactDensity] = useState(() => window.matchMedia(COMPACT_DENSITY_QUERY).matches)
  const isCompactDensity = displayDensity === 'compact' || viewportRequiresCompactDensity
  const [mailContentScale, setMailContentScale] = useState<MailContentScale>(loadMailContentScale)
  const [isMobileSidebarOpen, setMobileSidebarOpen] = useState(false)
  const [isSidebarCollapsed, setSidebarCollapsed] = useState(() => window.matchMedia(AUTO_COLLAPSE_SIDEBAR_QUERY).matches)
  const [isMobileAuthOpen, setMobileAuthOpen] = useState(false)
  const [query, setQuery] = useState('')
  const deferredQuery = useDeferredValue(query)
  const [filterUnread, setFilterUnread] = useState(false)
  const [isComposeOpen, setComposeOpen] = useState(false)
  const [composeDraftId, setComposeDraftId] = useState<string | undefined>()
  const [composeMode, setComposeMode] = useState<ComposeMode>('new')
  const [composeSource, setComposeSource] = useState<MailMessage | undefined>()
  const [isAccountModalOpen, setAccountModalOpen] = useState(false)
  const [isMailRulesOpen, setMailRulesOpen] = useState(false)
  const [mailRules, setMailRules] = useState<NativeMailRule[]>([])
  const [mailRuleBusyKey, setMailRuleBusyKey] = useState<string | null>(null)
  const [mailRuleError, setMailRuleError] = useState('')
  const [isAddingAccount, setAddingAccount] = useState(false)
  const [isAuthPanelOpen, setAuthPanelOpen] = useState(false)
  const [isSettingsOpen, setSettingsOpen] = useState(false)
  const [openMenu, setOpenMenu] = useState<ActionMenu | null>(null)
  const [isHelpOpen, setHelpOpen] = useState(false)
  const [pendingExternalLink, setPendingExternalLink] = useState<ExternalLinkInspection | null>(null)
  const [isSyncing, setSyncing] = useState(false)
  const [isLoadingEarlier, setLoadingEarlier] = useState(false)
  const [isMailboxHydrating, setMailboxHydrating] = useState(isNativeRuntime)
  const [loadingMessageId, setLoadingMessageId] = useState<string | null>(null)
  const [mailboxMeta, setMailboxMeta] = useState<Record<string, MailboxPagingMeta>>({})
  const mailboxMetaRef = useRef<Record<string, MailboxPagingMeta>>({})
  const backgroundStatusRefreshRunningRef = useRef(false)
  const [nativeFolders, setNativeFolders] = useState<Record<string, string[]>>({})
  const [nativeFolderLabels, setNativeFolderLabels] = useState<Record<string, Record<string, string>>>({})
  const [selectedNativeFolder, setSelectedNativeFolder] = useState<{ accountId: string; name: string } | null>(null)
  const [isHtmlMode, setHtmlMode] = useState(false)
  const [isImporting, setImporting] = useState(false)
  const [toasts, setToasts] = useState<Toast[]>([])
  const [minimizeToTray, setMinimizeToTray] = useState(true)
  const [offlineMode, setOfflineMode] = useState(loadOfflineMode)
  const [notificationsEnabled, setNotificationsEnabled] = useState(true)
  const [remoteImagesEnabled, setRemoteImagesEnabled] = useState(loadRemoteImages)
  const [hideAds, setHideAds] = useState(loadHideAds)
  const [undoSendSeconds, setUndoSendSeconds] = useState<UndoSendSeconds>(DEFAULT_UNDO_SEND_SECONDS)
  const [pendingOperations, setPendingOperations] = useState(0)
  const [outboxTotal, setOutboxTotal] = useState(0)
  const [outboxPaused, setOutboxPaused] = useState(0)
  const [outboxScheduled, setOutboxScheduled] = useState(0)
  const [outboxUndoable, setOutboxUndoable] = useState(0)
  const [nativeOutboxItems, setNativeOutboxItems] = useState<NativeOutboxItem[]>(() => isNativeRuntime ? [] : sampleOutboxItems)
  const [snoozedMails, setSnoozedMails] = useState<MailMessage[]>([])
  const [snoozeActionId, setSnoozeActionId] = useState<string | null>(null)
  const [outboxAction, setOutboxAction] = useState<OutboxAction | null>(null)
  const [pendingOutboxDiscard, setPendingOutboxDiscard] = useState<NativeOutboxItem | null>(null)
  const [cacheStats, setCacheStats] = useState<NativeCacheStats | null>(null)
  const [cacheStatsState, setCacheStatsState] = useState<'loading' | 'ready' | 'error'>(isNativeRuntime ? 'loading' : 'ready')
  const [nativeDrafts, setNativeDrafts] = useState<NativeDraft[]>([])
  const [provider, setProvider] = useState<Provider>('qq')
  const [accountEmail, setAccountEmail] = useState('')
  const [editingAccountId, setEditingAccountId] = useState<string | null>(null)
  const [authorizationCode, setAuthorizationCode] = useState('')
  const [oauthSessionId, setOauthSessionId] = useState('')
  const [oauthState, setOauthState] = useState('')
  const [deviceFlow, setDeviceFlow] = useState<DeviceFlowState | null>(null)
  const [showAuthorizationCode, setShowAuthorizationCode] = useState(false)
  const [customCss, setCustomCss] = useState(loadCustomCss)
  const sanitizedCustomCss = useMemo(() => sanitizeCustomCss(customCss), [customCss])
  const [customImapHost, setCustomImapHost] = useState('imap.example.com')
  const [customImapPort, setCustomImapPort] = useState('993')
  const [customImapSecurity, setCustomImapSecurity] = useState('tls')
  const [customSmtpHost, setCustomSmtpHost] = useState('smtp.example.com')
  const [customSmtpPort, setCustomSmtpPort] = useState('465')
  const [customSmtpSecurity, setCustomSmtpSecurity] = useState('tls')
  const [customAuthentication, setCustomAuthentication] = useState('password')
  const [connectionDiagnostics, setConnectionDiagnostics] = useState<Record<string, ConnectionDiagnosticViewState>>({})
  const [attachmentProgress, setAttachmentProgress] = useState<Record<string, number>>({})

  useEffect(() => {
    mailboxMetaRef.current = mailboxMeta
  }, [mailboxMeta])

  const rememberMailboxPage = useCallback((result: NativeMailboxResponse, preserveOlderCursor = false) => {
    const next = pagingMetaFromResponse(result)
    if (!result.mailbox || !next) return
    const key = nativeMailboxKey(result.mailbox.accountId, result.mailbox.folder)
    setMailboxMeta((current) => {
      const previous = current[key]
      if (preserveOlderCursor && previous?.oldestUid != null && next.oldestUid != null && previous.oldestUid < next.oldestUid) {
        const merged = {
          ...next,
          oldestUid: previous.oldestUid,
          localHasMore: previous.localHasMore,
          hasMore: previous.localHasMore || next.remoteHasMore,
        }
        return { ...current, [key]: merged }
      }
      return { ...current, [key]: next }
    })
  }, [])
  const importInputRef = useRef<HTMLInputElement>(null)
  const accountPrefillRef = useRef<string | null>(null)
  const oauthSessionIdRef = useRef('')
  const authAttemptRef = useRef(0)
  const isAddingAccountRef = useRef(false)
  const selectedMailRef = useRef<MailMessage | undefined>(undefined)
  const attachmentCancelsRef = useRef(new Map<string, () => void>())
  const toastTimersRef = useRef(new Map<number, number>())
  const messageHydrationRef = useRef(new Map<string, Promise<NativeMessageResponse>>())
  const mailListRef = useRef<HTMLDivElement>(null)
  const [nativeStateReady, setNativeStateReady] = useState(!isNativeRuntime)
  const [nativeStateError, setNativeStateError] = useState<string | null>(null)

  const openExternalUrl = async (url: string) => {
    if (isNativeRuntime) {
      await invoke('app.open_external', { url })
      return
    }
    const opened = window.open(url, '_blank', 'noopener,noreferrer')
    if (!opened) throw new Error('当前环境阻止了外部浏览器跳转')
  }

  const handleRenderedLinkClick = (event: React.MouseEvent<HTMLDivElement>) => {
    const anchor = event.target instanceof Element ? event.target.closest('a') : null
    const href = anchor?.getAttribute('href')?.trim()
    if (!anchor || !href || href.startsWith('#')) return
    event.preventDefault()
    try {
      setPendingExternalLink(inspectExternalLink(href, anchor.textContent ?? undefined))
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '已阻止不安全的邮件链接', 'error')
    }
  }

  useEffect(() => () => {
    attachmentCancelsRef.current.forEach((cancel) => cancel())
    attachmentCancelsRef.current.clear()
    toastTimersRef.current.forEach((timer) => window.clearTimeout(timer))
    toastTimersRef.current.clear()
  }, [])

  useEffect(() => {
    const mobileMedia = window.matchMedia(MOBILE_LAYOUT_QUERY)
    const compactMedia = window.matchMedia(COMPACT_DENSITY_QUERY)
    const collapseMedia = window.matchMedia(AUTO_COLLAPSE_SIDEBAR_QUERY)
    const updateMobileLayout = (event: MediaQueryListEvent) => {
      setMobileLayout(event.matches)
      if (!event.matches) setMobileSidebarOpen(false)
    }
    const updateCompactDensity = (event: MediaQueryListEvent) => setViewportRequiresCompactDensity(event.matches)
    const updateSidebarForDesktop = (event: MediaQueryListEvent) => {
      if (event.matches) setSidebarCollapsed(true)
    }
    mobileMedia.addEventListener('change', updateMobileLayout)
    compactMedia.addEventListener('change', updateCompactDensity)
    collapseMedia.addEventListener('change', updateSidebarForDesktop)
    return () => {
      mobileMedia.removeEventListener('change', updateMobileLayout)
      compactMedia.removeEventListener('change', updateCompactDensity)
      collapseMedia.removeEventListener('change', updateSidebarForDesktop)
    }
  }, [])

  const dismissToast = (id: number) => {
    const timer = toastTimersRef.current.get(id)
    if (timer != null) window.clearTimeout(timer)
    toastTimersRef.current.delete(id)
    setToasts((current) => current.filter((toast) => toast.id !== id))
  }

  const pushToast = (message: string, tone: ToastTone = 'info', options?: ToastOptions) => {
    const id = Date.now() + Math.random()
    const durationMs = Math.max(1_200, options?.durationMs ?? 3_600)
    setToasts((current) => [...current.slice(-2), { id, message, tone, durationMs, action: options?.action }])
    const timer = window.setTimeout(() => {
      toastTimersRef.current.delete(id)
      setToasts((current) => current.filter((toast) => toast.id !== id))
      options?.onExpire?.()
    }, durationMs)
    toastTimersRef.current.set(id, timer)
  }

  const markAccountNeedsReauth = (accountId: string, error: unknown) => {
    const message = error instanceof Error ? error.message : String(error)
    if (!/auth|credential|login|password|authorization/i.test(message)) return
    setAccounts((current) => current.map((account) => account.id === accountId
      ? { ...account, status: 'needs-auth' as const, lastSync: '等待重新授权' }
      : account))
  }

  const refreshPendingOperations = async (accountList: MailAccount[] = accounts) => {
    if (!isNativeRuntime) {
      setPendingOperations(0)
      return
    }
    try {
      const statuses = await mapWithConcurrency(accountList, ACCOUNT_IPC_CONCURRENCY, (account) => invoke<NativeQueueStatus>('sync.queue_status', { accountId: account.id }))
      setPendingOperations(statuses.reduce((total, status) => total + status.total, 0))
    } catch {
      // Queue status is telemetry for the local UI; a transient read failure must not interrupt mail actions.
    }
  }

  const refreshOutbox = async (accountList: MailAccount[] = accounts) => {
    if (!isNativeRuntime) {
      return
    }
    try {
      const snapshots = await mapWithConcurrency(accountList, ACCOUNT_IPC_CONCURRENCY, (account) => invoke<NativeOutboxSnapshot>('mail.outbox.snapshot', { accountId: account.id }))
      const items = snapshots.flatMap((snapshot) => snapshot.items).sort((left, right) => right.updatedAt - left.updatedAt)
      startTransition(() => setNativeOutboxItems(items))
      setOutboxTotal(snapshots.reduce((total, snapshot) => total + snapshot.status.total, 0))
      setOutboxPaused(snapshots.reduce((total, snapshot) => total + snapshot.status.paused, 0))
      setOutboxScheduled(snapshots.reduce((total, snapshot) => total + (snapshot.status.userScheduled
        ?? snapshot.items.filter((item) => item.state === 'scheduled' && Boolean(item.scheduledAt)).length), 0))
      setOutboxUndoable(snapshots.reduce((total, snapshot) => {
        const explicit = snapshot.status.userScheduled
          ?? snapshot.items.filter((item) => item.state === 'scheduled' && Boolean(item.scheduledAt)).length
        return total + (snapshot.status.undoable ?? Math.max(0, (snapshot.status.scheduled ?? 0) - explicit))
      }, 0))
    } catch {
      // The encrypted queue remains native-owned; a transient summary read must not interrupt mail actions.
    }
  }

  const refreshSnoozed = async (accountList: MailAccount[] = accounts) => {
    if (!isNativeRuntime) return
    try {
      const snapshot = await invoke<NativeSnoozeSnapshot>('mail.snooze.snapshot', {})
      const next = snoozeSnapshotToUi(snapshot, accountList)
      startTransition(() => setSnoozedMails((current) => {
        const currentById = new Map(current.map((mail) => [mail.id, mail]))
        return next.map((mail) => {
          const hydrated = currentById.get(mail.id)
          return hydrated && !mailNeedsBodyHydration(hydrated)
            ? { ...hydrated, snoozedUntil: mail.snoozedUntil }
            : mail
        })
      }))
    } catch {
      // Snooze metadata is isolated from mailbox hydration; retry on the next foreground refresh.
    }
  }

  useEffect(() => {
    if (isNativeRuntime) return
    setOutboxTotal(nativeOutboxItems.length)
    setOutboxPaused(nativeOutboxItems.filter((item) => item.state === 'paused').length)
    setOutboxScheduled(nativeOutboxItems.filter((item) => item.state === 'scheduled' && Boolean(item.scheduledAt)).length)
    setOutboxUndoable(nativeOutboxItems.filter((item) => item.state === 'scheduled' && !item.scheduledAt).length)
  }, [isNativeRuntime, nativeOutboxItems])

  const nextSnoozeWakeAt = useMemo(() => snoozedMails.reduce<number | undefined>((earliest, mail) => (
    mail.snoozedUntil == null ? earliest : earliest == null ? mail.snoozedUntil : Math.min(earliest, mail.snoozedUntil)
  ), undefined), [snoozedMails])

  useEffect(() => {
    if (nextSnoozeWakeAt == null) return
    let timer: number | undefined
    const scheduleCheck = () => {
      const remaining = nextSnoozeWakeAt * 1_000 - Date.now()
      if (remaining > SNOOZE_TIMER_RECHECK_MS) {
        timer = window.setTimeout(scheduleCheck, SNOOZE_TIMER_RECHECK_MS)
        return
      }
      timer = window.setTimeout(() => {
        const now = Date.now() / 1_000
        setSnoozedMails((current) => current.filter((mail) => (mail.snoozedUntil ?? 0) > now))
        if (isNativeRuntime) void refreshSnoozed()
      }, Math.max(0, remaining) + 50)
    }
    scheduleCheck()
    return () => {
      if (timer != null) window.clearTimeout(timer)
    }
  }, [isNativeRuntime, nextSnoozeWakeAt])

  const refreshNativeDrafts = async (accountList: MailAccount[] = accounts) => {
    if (!isNativeRuntime) {
      setNativeDrafts([])
      return
    }
    try {
      const drafts = await mapWithConcurrency(accountList, ACCOUNT_IPC_CONCURRENCY, (account) => invoke<NativeDraft[]>('drafts.list', { accountId: account.id }, 30_000))
      setNativeDrafts(drafts.flat().sort((left, right) => right.updatedAt - left.updatedAt))
    } catch {
      // A missing or unreadable draft cache must not make the inbox unavailable.
    }
  }

  const openCompose = (draftId?: string, mode: ComposeMode = 'new', source?: MailMessage) => {
    setComposeDraftId(draftId)
    setComposeMode(mode)
    setComposeSource(source)
    setComposeOpen(true)
  }

  const mapMailSources = (updater: (mail: MailMessage) => MailMessage) => {
    setMails((current) => current.map(updater))
    setLocalSearchMails((current) => current.map(updater))
    setServerSearchMails((current) => current.map(updater))
    setSnoozedMails((current) => current.map((mail) => {
      const updated = updater(mail)
      return mail.snoozedUntil != null && updated.snoozedUntil == null
        ? { ...updated, snoozedUntil: mail.snoozedUntil }
        : updated
    }))
  }

  const filterMailSources = (predicate: (mail: MailMessage) => boolean) => {
    setMails((current) => current.filter(predicate))
    setServerSearchMails((current) => current.filter(predicate))
  }

  const applyRuleSnapshot = (nextRules: NativeMailRule[]) => {
    setMailRules(nextRules)
    mapMailSources((mail) => applyMailRules(mail, nextRules))
  }

  const refreshMailRules = async () => {
    if (!isNativeRuntime) return
    try {
      const snapshot = await invoke<NativeMailRuleSnapshot>('mail.rules.list', {}, 15_000)
      applyRuleSnapshot(snapshot.rules ?? [])
      setMailRuleError('')
    } catch (error) {
      setMailRuleError(error instanceof Error ? error.message : '本机屏蔽规则暂时无法读取')
    }
  }

  const addMailRule = async (accountId: string | undefined, kind: MailRuleKind, value: string) => {
    if (mailRuleBusyKey) throw new Error('请等待当前规则操作完成')
    setMailRuleBusyKey('add')
    setMailRuleError('')
    try {
      if (isNativeRuntime) {
        const snapshot = await invoke<NativeMailRuleSnapshot>('mail.rules.add', {
          ...(accountId ? { accountId } : {}),
          kind,
          value,
        }, 15_000)
        applyRuleSnapshot(snapshot.rules ?? [])
      } else {
        const duplicate = mailRules.find((rule) => rule.accountId === accountId && rule.kind === kind && rule.value === value)
        const nextRules = duplicate ? mailRules : [{ id: `demo-rule-${Date.now()}`, accountId, kind, value, createdAt: Date.now() }, ...mailRules]
        applyRuleSnapshot(nextRules)
      }
      pushToast(kind === 'sender' ? '已屏蔽该发件人' : '已屏蔽该域名及其子域名', 'success')
    } catch (error) {
      const message = error instanceof Error ? error.message : '屏蔽规则保存失败'
      setMailRuleError(message)
      throw error
    } finally {
      setMailRuleBusyKey(null)
    }
  }

  const removeMailRule = async (rule: NativeMailRule) => {
    if (mailRuleBusyKey) return
    setMailRuleBusyKey(rule.id)
    setMailRuleError('')
    try {
      if (isNativeRuntime) {
        const snapshot = await invoke<NativeMailRuleSnapshot>('mail.rules.remove', { id: rule.id }, 15_000)
        applyRuleSnapshot(snapshot.rules ?? [])
      } else {
        applyRuleSnapshot(mailRules.filter((item) => item.id !== rule.id))
      }
      pushToast('屏蔽规则已移除', 'success')
    } catch (error) {
      setMailRuleError(error instanceof Error ? error.message : '屏蔽规则移除失败')
    } finally {
      setMailRuleBusyKey(null)
    }
  }

  const blockMail = async (mail: MailMessage, kind: MailRuleKind) => {
    if (mail.id === 'empty-mail') return
    const value = kind === 'sender' ? mail.from : domainFromSender(mail.from)
    setOpenMenu(null)
    if (!value) {
      pushToast('这封邮件没有可用于屏蔽的有效发件地址', 'error')
      return
    }
    try {
      await addMailRule(mail.accountId, kind, value)
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '屏蔽规则保存失败', 'error')
    }
  }

  const handleDraftChanged = useCallback((draft: NativeDraft) => {
    setNativeDrafts((current) => [...current.filter((item) => item.id !== draft.id), draft].sort((left, right) => right.updatedAt - left.updatedAt))
  }, [])

  const handleDraftRemoved = useCallback((draftId: string) => {
    setNativeDrafts((current) => current.filter((draft) => draft.id !== draftId))
  }, [])

  const cancelNativeAuthSession = (sessionId: string) => {
    if (!isNativeRuntime || !sessionId) return
    void invoke('auth.cancel', { sessionId }).catch(() => undefined)
  }

  const invalidateAuthFlow = () => {
    authAttemptRef.current += 1
    const sessionId = oauthSessionIdRef.current || oauthSessionId
    cancelNativeAuthSession(sessionId)
    oauthSessionIdRef.current = ''
    return authAttemptRef.current
  }

  const closeAccountModal = () => {
    if (isAddingAccountRef.current) return
    invalidateAuthFlow()
    setAccountModalOpen(false)
    setAuthorizationCode('')
    setShowAuthorizationCode(false)
    setOauthSessionId('')
    setOauthState('')
    setDeviceFlow(null)
  }

  const changeProvider = (nextProvider: Provider, allowLocked = false) => {
    if (isAccountModalOpen && editingAccountId && !allowLocked) return
    invalidateAuthFlow()
    setProvider(nextProvider)
    setCustomAuthentication(nextProvider === 'outlook' || nextProvider === 'google' ? 'oauth2' : nextProvider === 'other' ? 'password' : 'app-password')
    setAuthorizationCode('')
    setOauthSessionId('')
    setOauthState('')
    setDeviceFlow(null)
  }

  const openNewAccount = () => {
    setEditingAccountId(null)
    changeProvider('qq', true)
    setAccountEmail('')
    setAuthorizationCode('')
    setShowAuthorizationCode(false)
    setCustomImapHost('imap.example.com')
    setCustomImapPort('993')
    setCustomImapSecurity('tls')
    setCustomSmtpHost('smtp.example.com')
    setCustomSmtpPort('465')
    setCustomSmtpSecurity('tls')
    setAccountModalOpen(true)
  }

  useEffect(() => {
    oauthSessionIdRef.current = oauthSessionId
  }, [oauthSessionId])

  useEffect(() => {
    if (!isAccountModalOpen) {
      setAuthorizationCode('')
      setShowAuthorizationCode(false)
    }
  }, [isAccountModalOpen])

  useEffect(() => {
    if (!isNativeRuntime || !deviceFlow || deviceFlow.status !== 'pending') return
    let cancelled = false
    let timer: number | undefined
    const poll = async () => {
      try {
        const result = await invoke<{ status: 'pending' | 'complete'; retryAfter?: number }>('auth.device.poll', { sessionId: deviceFlow.sessionId }, 30_000)
        if (cancelled) return
        if (result.status === 'complete') {
          setDeviceFlow((current) => current?.sessionId === deviceFlow.sessionId ? { ...current, status: 'complete' } : current)
          pushToast('Outlook 设备授权已完成，可以开始同步', 'success')
          return
        }
        timer = window.setTimeout(poll, Math.max(5, result.retryAfter ?? deviceFlow.retryAfter) * 1000)
      } catch (error) {
        if (cancelled) return
        const message = error instanceof Error ? error.message : ''
        if (/expired|denied|missing|rejected/i.test(message)) {
          setDeviceFlow((current) => current?.sessionId === deviceFlow.sessionId ? { ...current, status: 'error', message: message || '设备授权已失效，请重新开始。' } : current)
          pushToast(message || '设备授权已失效，请重新开始', 'error')
          return
        }
        timer = window.setTimeout(poll, 10_000)
      }
    }
    timer = window.setTimeout(poll, Math.max(5, deviceFlow.retryAfter) * 1000)
    return () => {
      cancelled = true
      if (timer) window.clearTimeout(timer)
    }
  }, [deviceFlow?.sessionId, deviceFlow?.status, isNativeRuntime])

  useEffect(() => {
    document.documentElement.dataset.theme = theme
    writeLocalStorageValue('mailgo-theme', theme)
    if (isNativeRuntime && nativeStateReady) {
      void invoke('app.set_theme', { theme }).catch(() => undefined)
    }
  }, [isNativeRuntime, nativeStateReady, theme])

  useEffect(() => {
    writeLocalStorageValue('mailgo-display-density-v3', displayDensity)
  }, [displayDensity])

  useEffect(() => {
    writeLocalStorageValue('mailgo-mail-content-scale-v2', String(mailContentScale))
  }, [mailContentScale])

  useEffect(() => {
    writeLocalStorageValue('mailgo-offline-mode', String(offlineMode))
    if (isNativeRuntime && nativeStateReady) {
      void invoke('app.set_offline_mode', { enabled: offlineMode }).catch(() => undefined)
    }
  }, [isNativeRuntime, nativeStateReady, offlineMode])

  useEffect(() => {
    if (!isNativeRuntime) {
      setNativeStateReady(true)
      setMailboxHydrating(false)
      return
    }
    let cancelled = false
    void readNativeState().then(async (nativeState) => {
      if (cancelled) return
      if (!nativeState) {
        setNativeStateReady(true)
        setMailboxHydrating(false)
        return
      }
      setNativeStateError(null)
      if (isNativeRuntime) setAccounts(nativeState.accounts)
      if (isNativeRuntime) setNativeFolders(nativeState.folders ?? {})
      if (isNativeRuntime) setNativeFolderLabels(nativeState.folderLabels ?? {})
      if (isNativeRuntime) {
        setMails([])
        setServerSearchMails([])
        setSelectedMailId('')
      }
      setTheme(nativeState.theme)
      setMinimizeToTray(nativeState.minimizeToTray)
      setOfflineMode(nativeState.offlineMode ?? false)
      setNotificationsEnabled(nativeState.notificationsEnabled ?? true)
      setRemoteImagesEnabled(nativeState.remoteImagesEnabled ?? false)
      setHideAds(nativeState.hideAds ?? false)
      setUndoSendSeconds(asUndoSendSeconds(nativeState.undoSendSeconds))
      setNativeStateReady(true)
      void refreshPendingOperations(nativeState.accounts)
      void refreshOutbox(nativeState.accounts)
      void refreshSnoozed(nativeState.accounts)
      void refreshNativeDrafts(nativeState.accounts)
      void refreshMailRules()
      if (!isNativeRuntime) return
      let firstMailboxSettled = false
      if (!nativeState.accounts.length) setMailboxHydrating(false)
      await mapWithConcurrency(nativeState.accounts, ACCOUNT_IPC_CONCURRENCY, async (account) => {
        try {
          const result = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id, limit: INITIAL_MAILBOX_PAGE_SIZE })
          const converted = (result.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, account))
          if (cancelled) return
          rememberMailboxPage(result)
          startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, result.mailbox?.folder ?? 'INBOX', 'replace')))
          if (converted.length) setSelectedMailId((current) => !current || current === 'launch-plan' ? converted[0].id : current)
        } catch {
          // An empty cache is a valid first-run state; sync will populate it later.
        } finally {
          if (!cancelled && !firstMailboxSettled) {
            firstMailboxSettled = true
            setMailboxHydrating(false)
          }
        }
      })
      if (!cancelled) setMailboxHydrating(false)
    }).catch((error) => {
      if (cancelled) return
      setNativeStateError(error instanceof Error ? error.message : '无法连接本地邮件服务')
      setNativeStateReady(true)
      setMailboxHydrating(false)
      pushToast('本地邮件服务连接失败，账户与缓存没有被修改', 'error')
    })
    return () => { cancelled = true }
  }, [isNativeRuntime, rememberMailboxPage])

  useEffect(() => {
    if (!isNativeRuntime) return
    let cancelled = false
    const refreshBackgroundStatuses = async () => {
      if (backgroundStatusRefreshRunningRef.current) return
      backgroundStatusRefreshRunningRef.current = true
      try {
        const nativeState = await readNativeState()
        if (cancelled || !nativeState) return
        setNativeStateError(null)
        const refreshedAccounts = new Map(nativeState.accounts.map((account) => [account.id, account]))
        setAccounts((current) => {
          let changed = false
          const next = current.map((account) => {
            const refreshed = refreshedAccounts.get(account.id)
            if (!refreshed
              || (account.unread === refreshed.unread && account.status === refreshed.status && account.lastSync === refreshed.lastSync)) return account
            changed = true
            return { ...account, unread: refreshed.unread, status: refreshed.status, lastSync: refreshed.lastSync }
          })
          return changed ? next : current
        })
        const refreshedFolders = nativeState.folders ?? {}
        const refreshedFolderLabels = nativeState.folderLabels ?? {}
        setNativeFolders((current) => sameStringArrayRecord(current, refreshedFolders) ? current : refreshedFolders)
        setNativeFolderLabels((current) => sameNestedStringRecord(current, refreshedFolderLabels) ? current : refreshedFolderLabels)
        void refreshSnoozed(nativeState.accounts)
        await mapWithConcurrency(nativeState.accounts, ACCOUNT_IPC_CONCURRENCY, async (account) => {
          try {
            const folder = nativeFolderName(account, 'inbox')
            const knownRevision = mailboxMetaRef.current[nativeMailboxKey(account.id, folder)]?.revision
            const result = await invoke<NativeMailboxResponse>('mail.list', {
              accountId: account.id,
              folder,
              limit: INITIAL_MAILBOX_PAGE_SIZE,
              ...(knownRevision == null ? {} : { knownRevision }),
            })
            if (cancelled || result.unchanged || !result.mailbox) return
            const converted = result.mailbox.messages.map((message) => nativeMessageToUi(message, account))
            rememberMailboxPage(result, true)
            startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, result.mailbox!.folder, 'latest')))
            if (converted.length) setSelectedMailId((current) => !current || current === 'launch-plan' ? converted[0].id : current)
          } catch {
            // The background scheduler may still be committing this mailbox snapshot; retry on the next local refresh.
          }
        })
      } catch (error) {
        if (!cancelled) setNativeStateError(error instanceof Error ? error.message : '无法连接本地邮件服务')
      } finally {
        backgroundStatusRefreshRunningRef.current = false
      }
    }
    const initialTimer = window.setTimeout(() => { void refreshBackgroundStatuses() }, 4_000)
    const timer = window.setInterval(() => { void refreshBackgroundStatuses() }, 30_000)
    return () => {
      cancelled = true
      window.clearTimeout(initialTimer)
      window.clearInterval(timer)
    }
  }, [isNativeRuntime, rememberMailboxPage])

  useEffect(() => {
    if (!isNativeRuntime || !nativeStateReady) return
    let cancelled = false
    let timer: number | undefined
    let loadingPolls = 0

    const refreshCacheStats = async (refresh: boolean) => {
      try {
        const response = await invoke<NativeCacheStatsResponse>('app.cache_stats', { refresh })
        if (cancelled) return
        setCacheStatsState(response.state)
        if (response.stats) setCacheStats(response.stats)
        if (response.state === 'loading') {
          loadingPolls += 1
          timer = window.setTimeout(() => { void refreshCacheStats(false) }, Math.min(1_000, 120 + loadingPolls * 80))
        } else {
          loadingPolls = 0
          timer = window.setTimeout(() => { void refreshCacheStats(true) }, response.state === 'error' ? 30_000 : 60_000)
        }
      } catch {
        if (cancelled) return
        setCacheStatsState('error')
        loadingPolls = 0
        timer = window.setTimeout(() => { void refreshCacheStats(true) }, 30_000)
      }
    }

    void refreshCacheStats(true)
    return () => {
      cancelled = true
      if (timer) window.clearTimeout(timer)
    }
  }, [isNativeRuntime, nativeStateReady])

  const accountScopeKey = accounts.map((account) => `${account.id}:${account.provider}:${account.label}:${account.email}:${account.accent}`).join('|')
  const searchAccountDirectory = useMemo(() => new Map(accounts.map((account) => [account.id, account])), [accountScopeKey])

  useEffect(() => {
    const trimmedQuery = deferredQuery.trim()
    if (!isNativeRuntime || trimmedQuery.length < 2) {
      setLocalSearchMails([])
      setLocalSearchState('idle')
      setLocalSearchTruncated(false)
      return
    }
    if (!accountScopeKey) {
      setLocalSearchMails([])
      setLocalSearchState('ready')
      setLocalSearchTruncated(false)
      return
    }
    let cancelled = false
    let timer: number | undefined
    setLocalSearchMails([])
    setLocalSearchState('searching')
    setLocalSearchTruncated(false)

    const searchLocalCache = () => {
      void invoke<NativeLocalSearchResponse>('mail.search.local', {
        query: trimmedQuery,
        ...(selectedAccountId ? { accountId: selectedAccountId } : {}),
        limit: 240,
      }, 15_000).then((result) => {
        if (cancelled) return
        const nextMails = (result.messages ?? []).flatMap((message) => {
          const account = searchAccountDirectory.get(message.accountId)
          return account ? [nativeMessageToUi(message, account)] : []
        })
        startTransition(() => {
          setLocalSearchMails(nextMails)
          setLocalSearchTruncated(Boolean(result.truncated))
          setLocalSearchState(result.indexing ? 'indexing' : 'ready')
        })
        if (result.indexing) {
          timer = window.setTimeout(searchLocalCache, 260)
        }
      }).catch(() => {
        if (cancelled) return
        startTransition(() => {
          setLocalSearchMails([])
          setLocalSearchTruncated(false)
          setLocalSearchState('error')
        })
      })
    }

    timer = window.setTimeout(searchLocalCache, 80)
    return () => {
      cancelled = true
      if (timer) window.clearTimeout(timer)
    }
  }, [accountScopeKey, deferredQuery, isNativeRuntime, searchAccountDirectory, selectedAccountId])

  useEffect(() => {
    const trimmedQuery = deferredQuery.trim()
    if (!isNativeRuntime || offlineMode || trimmedQuery.length < 2) {
      setServerSearchMails([])
      setServerSearchState(offlineMode && trimmedQuery.length >= 2 ? 'ready' : 'idle')
      setServerSearchTruncated(false)
      return
    }
    if (!accountScopeKey) {
      setServerSearchMails([])
      setServerSearchState('ready')
      setServerSearchTruncated(false)
      return
    }
    let cancelled = false
    setServerSearchState('searching')
    const timer = window.setTimeout(() => {
      void invoke<NativeSearchResponse>('mail.search', {
        query: trimmedQuery,
        ...(selectedAccountId ? { accountId: selectedAccountId } : {}),
        limit: 240,
      }, 60_000).then((result) => {
        if (cancelled) return
        const nextMails = (result.messages ?? []).flatMap((message) => {
          const account = searchAccountDirectory.get(message.accountId)
          return account ? [nativeMessageToUi(message, account)] : []
        })
        startTransition(() => {
          setServerSearchMails(nextMails)
          setServerSearchTruncated(Boolean(result.truncated))
          setServerSearchState(result.failed?.length && !result.messages?.length ? 'error' : 'ready')
        })
      }).catch(() => {
        if (cancelled) return
        startTransition(() => {
          setServerSearchMails([])
          setServerSearchTruncated(false)
          setServerSearchState('error')
        })
      })
    }, 420)
    return () => {
      cancelled = true
      window.clearTimeout(timer)
    }
  }, [accountScopeKey, deferredQuery, isNativeRuntime, offlineMode, searchAccountDirectory, selectedAccountId])

  useEffect(() => {
    const styleId = 'mailgo-user-theme'
    let style = document.getElementById(styleId) as HTMLStyleElement | null
    if (!style) {
      style = document.createElement('style')
      style.id = styleId
      document.head.appendChild(style)
    }
    const boundedCss = sanitizedCustomCss.css.slice(0, MAX_CUSTOM_CSS_LENGTH)
    style.textContent = boundedCss
    writeLocalStorageValue('mailgo-custom-css', boundedCss)
  }, [sanitizedCustomCss])

  useEffect(() => {
    if (customCss.length > MAX_CUSTOM_CSS_LENGTH) {
      setCustomCss((current) => current.slice(0, MAX_CUSTOM_CSS_LENGTH))
    }
  }, [customCss])

  useEffect(() => {
    writeLocalStorageValue('mailgo-remote-images', String(remoteImagesEnabled))
  }, [remoteImagesEnabled])

  useEffect(() => {
    writeLocalStorageValue('mailgo-hide-ads', String(hideAds))
  }, [hideAds])

  useEffect(() => {
    if (!isAccountModalOpen) {
      accountPrefillRef.current = null
      return
    }
    if (!editingAccountId || accountPrefillRef.current === editingAccountId) return
    const account = accounts.find((item) => item.id === editingAccountId)
    if (!account) return
    setAccountEmail(account.email)
    accountPrefillRef.current = editingAccountId
  }, [accounts, editingAccountId, isAccountModalOpen])

  useEffect(() => {
    const closeMenus = (event: MouseEvent) => {
      if (!(event.target as Element | null)?.closest('.menu-anchor')) setOpenMenu(null)
    }
    document.addEventListener('mousedown', closeMenus)
    return () => document.removeEventListener('mousedown', closeMenus)
  }, [])

  useEffect(() => {
    const handleShortcut = (event: KeyboardEvent) => {
      if (document.querySelector('.external-link-modal')) {
        if (event.key === 'Escape') {
          event.preventDefault()
          setPendingExternalLink(null)
        }
        return
      }
      if (document.querySelector('.mail-rule-modal')) return
      const eventTarget = event.target instanceof HTMLElement ? event.target : null
      const isEditableTarget = Boolean(eventTarget?.matches('input, textarea, select, [contenteditable="true"]'))
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
        event.preventDefault()
        document.getElementById('mail-search')?.focus()
      }
      if (event.key === 'Escape' && !document.querySelector('.compose-modal')) {
        setComposeOpen(false)
        setComposeDraftId(undefined)
        setComposeMode('new')
        setComposeSource(undefined)
        closeAccountModal()
        setSettingsOpen(false)
        setOpenMenu(null)
        setHelpOpen(false)
      }
      if (!event.ctrlKey && !event.metaKey && event.key.toLowerCase() === 'c' && !isEditableTarget) {
        event.preventDefault()
        openCompose()
      }
      if (!event.ctrlKey && !event.metaKey && event.key.toLowerCase() === 'r' && !isEditableTarget && selectedMailRef.current?.id !== 'empty-mail' && !selectedMailRef.current?.outboxId) {
        event.preventDefault()
        if (selectedMailRef.current) openCompose(undefined, 'reply', selectedMailRef.current)
      }
    }
    window.addEventListener('keydown', handleShortcut)
    return () => window.removeEventListener('keydown', handleShortcut)
  }, [])

  const selectedProvider = providerFor(provider)
  const accountsById = useMemo(() => new Map(accounts.map((account) => [account.id, account])), [accounts])
  const queuedDraftKeys = useMemo(() => new Set(nativeOutboxItems.flatMap((item) => item.draftId ? [`${item.accountId}\u0000${item.draftId}`] : [])), [nativeOutboxItems])
  const visibleNativeDrafts = useMemo(() => nativeDrafts.filter((draft) => !queuedDraftKeys.has(`${draft.accountId}\u0000${draft.id}`)), [nativeDrafts, queuedDraftKeys])
  const localDraftMails = useMemo(() => visibleNativeDrafts.flatMap((draft) => {
    const account = accountsById.get(draft.accountId)
    return account ? [draftToUi(draft, account)] : []
  }), [accountsById, visibleNativeDrafts])
  const outboxMails = useMemo(() => nativeOutboxItems.flatMap((item) => {
    const account = accountsById.get(item.accountId)
    return account ? [outboxToUi(item, account)] : []
  }), [accountsById, nativeOutboxItems])
  const allMails = useMemo(() => {
    const merged = new Map<string, MailMessage>()
    for (const mail of localSearchMails) merged.set(mail.id, mail)
    for (const mail of serverSearchMails) merged.set(mail.id, mail)
    for (const mail of mails) merged.set(mail.id, mail)
    for (const mail of snoozedMails) {
      const existing = merged.get(mail.id)
      const preferred = existing && !mailNeedsBodyHydration(existing) ? existing : mail
      merged.set(mail.id, { ...preferred, snoozedUntil: mail.snoozedUntil })
    }
    for (const mail of localDraftMails) merged.set(mail.id, mail)
    for (const mail of outboxMails) merged.set(mail.id, mail)
    return [...merged.values()]
  }, [localDraftMails, localSearchMails, mails, outboxMails, serverSearchMails, snoozedMails])
  const nativeSearchResultIds = useMemo(
    () => new Set([...localSearchMails, ...serverSearchMails].map((mail) => mail.id)),
    [localSearchMails, serverSearchMails],
  )
  const unreadNativeFolderCounts = useMemo(() => {
    const counts = new Map<string, number>()
    for (const mail of allMails) {
      if (!mail.unread || mail.blocked || !mail.nativeFolder || mail.snoozedUntil != null) continue
      const key = nativeFolderCountKey(mail.accountId, mail.nativeFolder)
      counts.set(key, (counts.get(key) ?? 0) + 1)
    }
    return counts
  }, [allMails])
  const displayedFolderLabels = useMemo(() => {
    if (!isNativeRuntime) return folderLabels.map((folder) => folder.id === 'outbox'
      ? { ...folder, unread: nativeOutboxItems.length }
      : folder.id === 'snoozed'
        ? { ...folder, unread: snoozedMails.length }
        : folder.id === 'inbox'
          ? { ...folder, unread: Math.max(0, folder.unread - snoozedMails.filter((mail) => mail.unread).length) }
          : folder)
    const matchesFixedFolder = (mail: MailMessage, folder: FolderId) => {
      if (folder === 'starred') return mail.starred
      if (folder === 'snoozed') return Boolean(mail.snoozedUntil)
      if (folder === 'outbox') return mail.folder === 'outbox'
      const account = accountsById.get(mail.accountId)
      return mail.nativeFolder && account
        ? isSameNativeFolder(mail.nativeFolder, nativeFolderName(account, folder))
        : mail.folder === folder
    }
    return folderLabels.map((folder) => ({
      ...folder,
      unread: folder.id === 'outbox'
        ? nativeOutboxItems.length
        : folder.id === 'snoozed'
          ? snoozedMails.length
        : folder.id === 'drafts'
          ? visibleNativeDrafts.length
        : allMails.filter((mail) => !mail.blocked && mail.snoozedUntil == null && matchesFixedFolder(mail, folder.id) && mail.unread).length,
    }))
  }, [accountsById, allMails, isNativeRuntime, nativeOutboxItems.length, snoozedMails.length, visibleNativeDrafts.length])
  const displayedAccountUnreadCounts = useMemo(() => {
    const hiddenUnread = new Map<string, number>()
    for (const mail of allMails) {
      if (mail.unread && (mail.blocked || mail.snoozedUntil != null)) {
        hiddenUnread.set(mail.accountId, (hiddenUnread.get(mail.accountId) ?? 0) + 1)
      }
    }
    return new Map(accounts.map((account) => [account.id, Math.max(0, account.unread - (hiddenUnread.get(account.id) ?? 0))]))
  }, [accounts, allMails])
  const visibleMails = useMemo(() => {
    const lowerQuery = deferredQuery.trim().toLowerCase()
    return allMails.filter((mail) => {
      const nativeFolder = mail.nativeFolder
      const snoozed = mail.snoozedUntil != null
      const folderMatch = selectedNativeFolder
        ? mail.accountId === selectedNativeFolder.accountId && typeof nativeFolder === 'string' && isSameNativeFolder(nativeFolder, selectedNativeFolder.name)
        : selectedFolder === 'starred'
          ? mail.starred
          : selectedFolder === 'snoozed'
            ? snoozed
          : selectedFolder === 'outbox'
            ? mail.folder === 'outbox'
          : (() => {
              const account = accountsById.get(mail.accountId)
              return nativeFolder && account
                ? isSameNativeFolder(nativeFolder, nativeFolderName(account, selectedFolder))
                : mail.folder === selectedFolder
            })()
      const accountMatch = selectedNativeFolder ? true : !selectedAccountId || mail.accountId === selectedAccountId
      const categoryMatch = !selectedCategory || (selectedCategory === 'ads'
        ? mail.blocked || mail.category === 'ads' || mail.category === 'apple-ads'
        : mail.category === selectedCategory)
      const adMatch = !hideAds || !mail.isAd || Boolean(selectedCategory)
      const blockMatch = !mail.blocked || selectedCategory === 'ads'
      const unreadMatch = !filterUnread || mail.unread
      const queryMatch = !lowerQuery
        || nativeSearchResultIds.has(mail.id)
        || `${mail.senderName} ${mail.subject} ${mail.preview}`.toLowerCase().includes(lowerQuery)
      const snoozeMatch = selectedFolder === 'snoozed' ? snoozed : !snoozed
      return folderMatch && snoozeMatch && accountMatch && categoryMatch && adMatch && blockMatch && unreadMatch && queryMatch
    })
  }, [accountsById, allMails, deferredQuery, filterUnread, hideAds, nativeSearchResultIds, selectedAccountId, selectedCategory, selectedFolder, selectedNativeFolder])

  const visibleThreads = useMemo(() => buildMailThreads(visibleMails), [visibleMails])

  const selectedMail = visibleMails.find((mail) => mail.id === selectedMailId) ?? visibleMails[0] ?? {
    id: 'empty-mail',
    accountId: '',
    folder: selectedFolder,
    from: '',
    senderName: 'MailGo',
    subject: '没有选中的邮件',
    preview: '当前文件夹没有可显示的邮件。',
    timestamp: '',
    dateGroup: '',
    unread: false,
    starred: false,
    accent: '#8b99b6',
    avatar: 'M',
    body: ['当前文件夹没有可显示的邮件。'],
  } satisfies MailMessage
  selectedMailRef.current = selectedMail
  useEffect(() => {
    setHtmlMode(Boolean(selectedMail.hasHtml))
  }, [selectedMail.hasHtml, selectedMail.id])
  const selectedThread = visibleThreads.find((thread) => thread.messages.some((mail) => mail.id === selectedMail.id)) ?? visibleThreads[0]
  const selectedMailAccount = accountsById.get(selectedMail.accountId)
  const selectedOutboxItem = selectedMail.outboxId
    ? nativeOutboxItems.find((item) => item.id === selectedMail.outboxId && item.accountId === selectedMail.accountId)
    : undefined
  const selectedMailMoveTargets = useMemo(() => {
    if (!isNativeRuntime || !selectedMailAccount || selectedMail.nativeUid == null) return []
    return nativeMoveTargets(selectedMailAccount, nativeFolders[selectedMailAccount.id], nativeFolderLabels[selectedMailAccount.id])
      .filter((target) => !selectedMail.nativeFolder || !isSameNativeFolder(target.folder, selectedMail.nativeFolder))
  }, [isNativeRuntime, nativeFolderLabels, nativeFolders, selectedMail.nativeFolder, selectedMail.nativeUid, selectedMailAccount])

  const groupedThreads = useMemo(() => {
    return visibleThreads.reduce<Record<string, MailThread[]>>((groups, thread) => {
      groups[thread.latest.dateGroup] ??= []
      groups[thread.latest.dateGroup].push(thread)
      return groups
    }, {})
  }, [visibleThreads])

  const virtualMailItems = useMemo<VirtualMailItem[]>(() => Object.entries(groupedThreads).flatMap(([group, groupThreads]) => [
    { type: 'group' as const, key: `group:${group}`, label: group },
    ...groupThreads.map((thread) => ({ type: 'thread' as const, key: `thread:${thread.key}`, thread })),
  ]), [groupedThreads])
  const getVirtualMailKey = useCallback(
    (index: number) => virtualMailItems[index]?.key ?? `mail-index:${index}`,
    [virtualMailItems],
  )
  const mailListVirtualizer = useVirtualizer({
    count: virtualMailItems.length,
    getScrollElement: () => mailListRef.current,
    estimateSize: (index) => virtualMailItems[index]?.type === 'group'
      ? (isMobileLayout ? MOBILE_MAIL_GROUP_HEIGHT : isCompactDensity ? COMPACT_MAIL_GROUP_HEIGHT : COMFORTABLE_MAIL_GROUP_HEIGHT)
      : (isMobileLayout ? MOBILE_MAIL_ROW_HEIGHT : isCompactDensity ? COMPACT_MAIL_ROW_HEIGHT : COMFORTABLE_MAIL_ROW_HEIGHT),
    getItemKey: getVirtualMailKey,
    overscan: 8,
    useFlushSync: false,
  })

  useEffect(() => {
    mailListVirtualizer.measure()
  }, [isCompactDensity, isMobileLayout, mailListVirtualizer])

  const selectedVisibleMails = useMemo(
    () => visibleMails.filter((mail) => selectedMailIds.includes(mail.id)),
    [selectedMailIds, visibleMails],
  )
  const allVisibleSelected = visibleMails.length > 0 && selectedVisibleMails.length === visibleMails.length

  const canLoadEarlier = isNativeRuntime && selectedFolder !== 'starred' && selectedFolder !== 'snoozed' && selectedFolder !== 'outbox' && (
    !selectedNativeFolder && accounts
      .filter((account) => !selectedAccountId || account.id === selectedAccountId)
      .some((account) => mailboxMeta[nativeMailboxKey(account.id, nativeFolderName(account, selectedFolder))]?.hasMore)
    || Boolean(selectedNativeFolder) && Boolean(mailboxMeta[nativeMailboxKey(selectedNativeFolder!.accountId, selectedNativeFolder!.name)]?.hasMore)
  )

  const requestNativeMessage = useCallback((mail: MailMessage) => {
    const existing = messageHydrationRef.current.get(mail.id)
    if (existing) return existing
    const request = invoke<NativeMessageResponse>('mail.get', {
      accountId: mail.accountId,
      folder: mail.nativeFolder ?? 'INBOX',
      uid: mail.nativeUid,
    }, 60_000)
    messageHydrationRef.current.set(mail.id, request)
    request.then(
      () => messageHydrationRef.current.delete(mail.id),
      () => messageHydrationRef.current.delete(mail.id),
    )
    return request
  }, [])

  const bodyHydrationCandidates = useMemo(
    () => selectBodyHydrationCandidates(visibleMails, selectedMail.id),
    [selectedMail.id, visibleMails],
  )

  const selectMail = async (mail: MailMessage) => {
    setSelectedMailId(mail.id)
    setMobilePane('reading')
    setLoadingMessageId(null)
    if (mail.outboxId) {
      setSelectedAccountId(mail.accountId)
      return
    }
    const localDraft = nativeDrafts.find((draft) => mail.id === `local-draft:${draft.accountId}:${draft.id}`)
    if (localDraft) {
      setSelectedAccountId(localDraft.accountId)
      openCompose(localDraft.id)
      return
    }
    if (mail.unread) mapMailSources((item) => item.id === mail.id ? { ...item, unread: false } : item)
    if (isNativeRuntime && mail.nativeUid) {
      if (!offlineMode) {
        void invoke('mail.mark_read', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: false })
          .then(() => refreshPendingOperations())
          .catch((error) => {
            markAccountNeedsReauth(mail.accountId, error)
            mapMailSources((item) => item.id === mail.id ? { ...item, unread: true } : item)
            setAccounts((current) => current.map((account) => account.id === mail.accountId
              ? { ...account, unread: account.unread + 1 }
              : account))
            pushToast('邮件仍未标记为已读，请重新授权或稍后重试', 'error')
          })
      }
      if (!mailNeedsBodyHydration(mail)) return
      setLoadingMessageId(mail.id)
      try {
        const result = await requestNativeMessage(mail)
        const account = accountsById.get(mail.accountId)
        if (account && result.message) {
          const converted = nativeMessageToUi(result.message, account)
          mapMailSources((item) => item.id === mail.id ? converted : item)
        }
      } catch {
        pushToast(offlineMode ? '这封邮件的正文尚未缓存，关闭仅离线模式后可下载' : '邮件正文加载失败，仍可查看本地摘要', 'info')
      } finally {
        setLoadingMessageId((current) => current === mail.id ? null : current)
      }
    }
  }

  const snoozeMail = async (mail: MailMessage, timestamp: number) => {
    if (mail.id === 'empty-mail' || mail.outboxId) throw new Error('这封邮件不能稍后处理')
    if (isNativeRuntime && (mail.nativeUid == null || !mail.accountId)) {
      throw new Error('邮件尚未进入本地索引，请同步后重试')
    }
    setSnoozeActionId(mail.id)
    try {
      const wakeAt = Math.floor(timestamp / 1_000)
      if (isNativeRuntime) {
        const snapshot = await invoke<NativeSnoozeSnapshot>('mail.snooze', {
          accountId: mail.accountId,
          folder: mail.nativeFolder ?? 'INBOX',
          uid: mail.nativeUid,
          wakeAt: timestamp,
        })
        const next = snoozeSnapshotToUi(snapshot, accounts)
          .map((item) => item.id === mail.id ? { ...mail, snoozedUntil: item.snoozedUntil } : item)
        startTransition(() => setSnoozedMails(next))
      } else {
        setSnoozedMails((current) => [
          ...current.filter((item) => item.id !== mail.id),
          { ...mail, snoozedUntil: wakeAt },
        ])
      }
      pushToast(`已稍后处理到 ${formatSnoozeTime(timestamp)}`, 'success')
    } finally {
      setSnoozeActionId((current) => current === mail.id ? null : current)
    }
  }

  const unsnoozeMail = async (mail: MailMessage) => {
    if (!mail.snoozedUntil) return
    setSnoozeActionId(mail.id)
    try {
      if (isNativeRuntime) {
        if (mail.nativeUid == null) throw new Error('邮件本地身份无效')
        const snapshot = await invoke<NativeSnoozeSnapshot>('mail.unsnooze', {
          accountId: mail.accountId,
          folder: mail.nativeFolder ?? 'INBOX',
          uid: mail.nativeUid,
        })
        startTransition(() => setSnoozedMails(snoozeSnapshotToUi(snapshot, accounts)))
      } else {
        setSnoozedMails((current) => current.filter((item) => item.id !== mail.id))
      }
      pushToast('邮件已回到收件箱', 'success')
    } finally {
      setSnoozeActionId((current) => current === mail.id ? null : current)
    }
  }

  useEffect(() => {
    if (!isNativeRuntime || offlineMode || bodyHydrationCandidates.length === 0) return
    const candidate = bodyHydrationCandidates[0]
    const isForeground = candidate.id === selectedMail.id
    const idleScheduler = window as unknown as {
      requestIdleCallback?: (callback: IdleRequestCallback, options?: IdleRequestOptions) => number
      cancelIdleCallback?: (handle: number) => void
    }
    let disposed = false
    let timer: number | undefined
    let idleRequest: number | undefined

    const hydrate = async () => {
      if (disposed) return
      if (isForeground) setLoadingMessageId(candidate.id)
      try {
        const result = await requestNativeMessage(candidate)
        if (disposed || !result.message) return
        const account = accountsById.get(candidate.accountId)
        if (!account) return
        const converted = nativeMessageToUi(result.message, account)
        startTransition(() => {
          setMails((current) => current.map((mail) => mail.id === candidate.id ? converted : mail))
          setServerSearchMails((current) => current.map((mail) => mail.id === candidate.id ? converted : mail))
          setSnoozedMails((current) => current.map((mail) => mail.id === candidate.id
            ? { ...converted, snoozedUntil: mail.snoozedUntil }
            : mail))
        })
      } catch {
        // Read-ahead is opportunistic. An explicit selection keeps its own visible retry path.
      } finally {
        if (!disposed && isForeground) {
          setLoadingMessageId((current) => current === candidate.id ? null : current)
        }
      }
    }

    if (isForeground) {
      timer = window.setTimeout(() => { void hydrate() }, 40)
    } else if (idleScheduler.requestIdleCallback) {
      idleRequest = idleScheduler.requestIdleCallback(() => { void hydrate() }, { timeout: 800 })
    } else {
      timer = window.setTimeout(() => { void hydrate() }, 320)
    }
    return () => {
      disposed = true
      if (timer != null) window.clearTimeout(timer)
      if (idleRequest != null) idleScheduler.cancelIdleCallback?.(idleRequest)
    }
  }, [accountsById, bodyHydrationCandidates, isNativeRuntime, offlineMode, requestNativeMessage, selectedMail.id])

  const toggleStar = (mail: MailMessage) => {
    const nextStarred = !mail.starred
    mapMailSources((item) => item.id === mail.id ? { ...item, starred: nextStarred } : item)
    setSelectedMailId(mail.id)
    if (isNativeRuntime && mail.nativeUid) {
      void invoke('mail.star', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: nextStarred })
        .then(() => refreshPendingOperations())
        .catch((error) => {
          markAccountNeedsReauth(mail.accountId, error)
          mapMailSources((item) => item.id === mail.id ? { ...item, starred: mail.starred } : item)
          pushToast('星标同步失败，已恢复原状态', 'error')
        })
    }
    pushToast(nextStarred ? '已添加到星标' : '已移出星标', 'success')
  }

  const toggleThreadSelection = (thread: MailThread) => {
    const threadIds = thread.messages.map((mail) => mail.id)
    const threadIdSet = new Set(threadIds)
    setSelectedMailIds((current) => threadIds.every((id) => current.includes(id))
      ? current.filter((id) => !threadIdSet.has(id))
      : Array.from(new Set([...current, ...threadIds])))
  }

  const toggleAllVisible = () => {
    setSelectedMailIds((current) => {
      if (allVisibleSelected) {
        const visibleIds = new Set(visibleMails.map((mail) => mail.id))
        return current.filter((id) => !visibleIds.has(id))
      }
      return Array.from(new Set([...current, ...visibleMails.map((mail) => mail.id)]))
    })
  }

  const setMailReadState = async (mail: MailMessage, unread: boolean) => {
    if (mail.id === 'empty-mail' || mail.unread === unread) return
    mapMailSources((item) => item.id === mail.id ? { ...item, unread } : item)
    setAccounts((current) => current.map((account) => account.id === mail.accountId
      ? { ...account, unread: Math.max(0, account.unread + (unread ? 1 : -1)) }
      : account))
    if (isNativeRuntime && mail.nativeUid) {
      try {
        await invoke('mail.mark_read', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: unread })
        await refreshPendingOperations()
      } catch (error) {
        markAccountNeedsReauth(mail.accountId, error)
        mapMailSources((item) => item.id === mail.id ? { ...item, unread: mail.unread } : item)
        setAccounts((current) => current.map((account) => account.id === mail.accountId
          ? { ...account, unread: Math.max(0, account.unread + (mail.unread ? 1 : -1)) }
          : account))
        throw error
      }
    }
  }

  const setSelectedReadState = async (unread: boolean) => {
    if (!selectedVisibleMails.length) {
      pushToast('请先选择邮件', 'info')
      return
    }
    const selected = selectedVisibleMails.filter((mail) => mail.unread !== unread)
    const selectedIds = new Set(selected.map((mail) => mail.id))
    mapMailSources((mail) => selectedIds.has(mail.id) ? { ...mail, unread } : mail)
    setAccounts((current) => current.map((account) => {
      const count = selected.filter((mail) => mail.accountId === account.id).length
      return count ? { ...account, unread: Math.max(0, account.unread + (unread ? count : -count)) } : account
    }))
    let failed = 0
    if (isNativeRuntime) {
      const results = await mapWithConcurrency(selected, BULK_ACTION_IPC_CONCURRENCY, async (mail) => {
        if (!mail.nativeUid) return { mail, ok: true }
        try {
          await invoke('mail.mark_read', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: unread })
          return { mail, ok: true }
        } catch (error) {
          markAccountNeedsReauth(mail.accountId, error)
          return { mail, ok: false }
        }
      })
      const failedMails = results.filter((result) => !result.ok).map((result) => result.mail)
      failed = failedMails.length
      if (failedMails.length) {
        const failedState = new Map(failedMails.map((mail) => [mail.id, mail]))
        mapMailSources((currentMail) => {
          const previous = failedState.get(currentMail.id)
          return previous ? { ...currentMail, unread: previous.unread } : currentMail
        })
        setAccounts((current) => current.map((account) => {
          const count = failedMails.filter((mail) => mail.accountId === account.id)
            .reduce((total, mail) => total + (mail.unread ? 1 : -1), 0)
          return count ? { ...account, unread: Math.max(0, account.unread + count) } : account
        }))
      }
      await refreshPendingOperations()
    }
    setSelectedMailIds([])
    setOpenMenu(null)
    const action = unread ? '标为未读' : '标为已读'
    if (failed) pushToast(`${selected.length - failed} 封已${unread ? '标未读' : '读'}，${failed} 封同步失败`, 'error')
    else pushToast(selected.length ? `已将 ${selected.length} 封邮件${action}` : `所选邮件已经${unread ? '是未读' : '是已读'}状态`, 'success')
  }

  const setSelectedStarred = async (starred: boolean) => {
    if (!selectedVisibleMails.length) {
      pushToast('请先选择邮件', 'info')
      return
    }
    const selected = selectedVisibleMails.filter((mail) => mail.starred !== starred)
    const selectedIds = new Set(selected.map((mail) => mail.id))
    mapMailSources((mail) => selectedIds.has(mail.id) ? { ...mail, starred } : mail)
    let failed = 0
    if (isNativeRuntime) {
      const results = await mapWithConcurrency(selected, BULK_ACTION_IPC_CONCURRENCY, async (mail) => {
        if (!mail.nativeUid) return { mail, ok: true }
        try {
          await invoke('mail.star', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: starred })
          return { mail, ok: true }
        } catch (error) {
          markAccountNeedsReauth(mail.accountId, error)
          return { mail, ok: false }
        }
      })
      const failedMails = results.filter((result) => !result.ok).map((result) => result.mail)
      failed = failedMails.length
      if (failedMails.length) {
        const failedState = new Map(failedMails.map((mail) => [mail.id, mail]))
        mapMailSources((currentMail) => {
          const previous = failedState.get(currentMail.id)
          return previous ? { ...currentMail, starred: previous.starred } : currentMail
        })
      }
      await refreshPendingOperations()
    }
    setSelectedMailIds([])
    setOpenMenu(null)
    if (failed) pushToast(`${selected.length - failed} 封已处理，${failed} 封星标同步失败`, 'error')
    else pushToast(selected.length ? `已${starred ? '添加' : '移除'} ${selected.length} 封邮件的星标` : `所选邮件已经${starred ? '全部加星' : '全部取消星标'}`, 'success')
  }

  const moveMail = async (mail: MailMessage, operation: 'archive' | 'delete' | 'spam' | 'inbox') => {
    if (mail.id === 'empty-mail') return false
    const account = accountsById.get(mail.accountId)
    if (operation === 'archive' && mail.folder === 'archive') return false
    if (operation === 'spam' && mail.folder === 'spam') return false
    if (operation === 'inbox' && mail.folder === 'inbox') return false
    const isPermanentDelete = operation === 'delete' && mail.folder === 'trash'
    const targetFolder = isPermanentDelete || !account ? undefined : nativeFolderName(account, operation === 'archive' ? 'archive' : operation === 'spam' ? 'spam' : operation === 'inbox' ? 'inbox' : 'trash')
    let queued = false
    if (isNativeRuntime && account && mail.nativeUid != null) {
      const command = operation === 'spam' ? 'mail.spam' : operation === 'inbox' ? 'mail.inbox' : 'mail.' + operation
      const result = await invoke<{ queued?: boolean }>(command, {
        accountId: mail.accountId,
        folder: mail.nativeFolder ?? 'INBOX',
        uid: mail.nativeUid,
        ...(targetFolder ? { targetFolder } : {}),
      })
      queued = Boolean(result.queued)
      await refreshPendingOperations()
    }
    if (isPermanentDelete) {
      filterMailSources((item) => item.id !== mail.id)
      if (selectedMailId === mail.id) {
        const next = visibleMails.find((item) => item.id !== mail.id)
        setSelectedMailId(next?.id ?? '')
      }
    } else {
      const nextFolder = operation === 'archive' ? 'archive' : operation === 'spam' ? 'spam' : operation === 'inbox' ? 'inbox' : 'trash'
      mapMailSources((item) => item.id === mail.id
        ? { ...item, folder: nextFolder, nativeFolder: targetFolder ?? item.nativeFolder }
        : item)
    }
    setSnoozedMails((current) => current.filter((item) => item.id !== mail.id))
    if (mail.unread && mail.folder === 'inbox') {
      setAccounts((current) => current.map((item) => item.id === mail.accountId
        ? { ...item, unread: Math.max(0, item.unread - 1) }
        : item))
    }
    return queued
  }

  const moveMailToFolder = async (mail: MailMessage, targetFolder: string) => {
    if (mail.id === 'empty-mail' || !isNativeRuntime || mail.nativeUid == null) return false
    const account = accountsById.get(mail.accountId)
    if (!account || !targetFolder.trim()) return false
    const result = await invoke<{ queued?: boolean }>('mail.move', {
      accountId: mail.accountId,
      folder: mail.nativeFolder ?? 'INBOX',
      uid: mail.nativeUid,
      targetFolder,
    })
    await refreshPendingOperations()
    mapMailSources((item) => item.id === mail.id
      ? { ...item, folder: uiFolderForNative(targetFolder), nativeFolder: targetFolder }
      : item)
    if (mail.unread && mail.folder === 'inbox' && !isSameNativeFolder(targetFolder, nativeFolderName(account, 'inbox'))) {
      setAccounts((current) => current.map((item) => item.id === mail.accountId
        ? { ...item, unread: Math.max(0, item.unread - 1) }
        : item))
    }
    return Boolean(result.queued)
  }

  const runMoveToFolder = async (mail: MailMessage, target: MailMoveTarget) => {
    try {
      const queued = await moveMailToFolder(mail, target.folder)
      pushToast(queued ? '操作已保存，联网后会自动同步' : `邮件已移入${target.label}`, 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '邮件操作失败，请稍后重试', 'error')
    }
  }

  const runMove = async (mail: MailMessage, operation: 'archive' | 'delete' | 'spam' | 'inbox') => {
    try {
      const queued = await moveMail(mail, operation)
      pushToast(queued ? '操作已保存，联网后会自动同步' : operation === 'archive' ? '邮件已归档' : operation === 'spam' ? '邮件已移入垃圾邮件' : operation === 'inbox' ? '邮件已移回收件箱' : '邮件已移入回收站', 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '邮件操作失败，请稍后重试', 'error')
    }
  }

  const applyBulkMove = async (operation: 'archive' | 'delete' | 'spam' | 'inbox') => {
    if (!selectedVisibleMails.length) {
      pushToast('请先选择邮件', 'info')
      return
    }
    let failed = 0
    let queued = 0
    for (const mail of selectedVisibleMails) {
      try {
        if (await moveMail(mail, operation)) queued += 1
      } catch {
        failed += 1
      }
    }
    const count = selectedVisibleMails.length - failed
    setSelectedMailIds([])
    if (failed) pushToast(`${count} 封邮件已处理，${failed} 封处理失败`, 'error')
    else pushToast(`${operation === 'archive' ? `已归档 ${count} 封邮件` : operation === 'spam' ? `已将 ${count} 封邮件移入垃圾邮件` : operation === 'inbox' ? `已将 ${count} 封邮件移回收件箱` : `已将 ${count} 封邮件移入回收站`}${queued ? `，${queued} 封将在联网后同步` : ''}`, 'success')
  }

  const markSelectedRead = () => { void setSelectedReadState(false) }

  const markSelectedUnread = () => { void setSelectedReadState(true) }

  const markSelectedMessageUnread = async () => {
    try {
      await setMailReadState(selectedMail, true)
      setOpenMenu(null)
      pushToast('邮件已标为未读', 'success')
    } catch {
      pushToast('标记未读失败，请稍后重试', 'error')
    }
  }

  const copySelectedMessage = async () => {
    try {
      await navigator.clipboard.writeText(`${selectedMail.subject}\n\n${selectedMail.body.join('\n\n')}`)
      setOpenMenu(null)
      pushToast('邮件正文已复制', 'success')
    } catch {
      pushToast('当前环境不允许访问剪贴板', 'error')
    }
  }

  const cancelAttachment = (attachmentId: string) => {
    attachmentCancelsRef.current.get(attachmentId)?.()
  }

  const downloadAttachment = async (attachment: MailAttachment) => {
    if (!isNativeRuntime || selectedMail.nativeUid == null || attachment.nativeIndex == null) {
      pushToast(`${attachment.name} 已加入下载队列`, 'success')
      return
    }
    if (attachmentCancelsRef.current.has(attachment.id)) {
      cancelAttachment(attachment.id)
      return
    }
    const controller = new AbortController()
    let downloadId: string | undefined
    const cancel = () => {
      controller.abort()
      if (downloadId) void invoke('mail.attachment.cancel', { downloadId }).catch(() => undefined)
    }
    attachmentCancelsRef.current.set(attachment.id, cancel)
    setAttachmentProgress((current) => ({ ...current, [attachment.id]: 0 }))
    try {
      const start = await invoke<NativeAttachmentStartResponse>('mail.attachment.start', {
        accountId: selectedMail.accountId,
        folder: selectedMail.nativeFolder ?? 'INBOX',
        uid: selectedMail.nativeUid,
        index: attachment.nativeIndex,
      }, 60_000)
      downloadId = start.downloadId
      if (controller.signal.aborted) {
        void invoke('mail.attachment.cancel', { downloadId }).catch(() => undefined)
        throw new Error('下载已取消')
      }
      let offset = 0
      let total = 0
      const parts: Uint8Array[] = []
      while (true) {
        if (controller.signal.aborted) throw new Error('下载已取消')
        const chunk = await invoke<NativeAttachmentChunkResponse>('mail.attachment.chunk', { downloadId, offset }, 60_000)
        if (chunk.downloadId !== start.downloadId || chunk.offset !== offset || chunk.nextOffset < offset || chunk.nextOffset > start.size || (!chunk.done && chunk.nextOffset === offset)) {
          throw new Error('附件传输响应无效')
        }
        const bytes = Uint8Array.from(atob(chunk.dataBase64), (character) => character.charCodeAt(0))
        parts.push(bytes)
        total += bytes.length
        if (total > start.size || chunk.nextOffset - offset !== bytes.length) {
          throw new Error('附件传输大小校验失败')
        }
        offset = chunk.nextOffset
        setAttachmentProgress((current) => ({ ...current, [attachment.id]: start.size ? Math.min(100, Math.round((offset / start.size) * 100)) : 100 }))
        if (chunk.done) break
      }
      const binary = new Uint8Array(total)
      let writeOffset = 0
      for (const part of parts) {
        binary.set(part, writeOffset)
        writeOffset += part.length
      }
      const url = URL.createObjectURL(new Blob([binary], { type: start.contentType }))
      const anchor = document.createElement('a')
      anchor.href = url
      anchor.download = start.fileName
      anchor.click()
      URL.revokeObjectURL(url)
      pushToast(`${start.fileName} 下载完成`, 'success')
    } catch (error) {
      if (downloadId) void invoke('mail.attachment.cancel', { downloadId }).catch(() => undefined)
      if (controller.signal.aborted) pushToast(`${attachment.name} 下载已取消`, 'info')
      else pushToast(error instanceof Error ? error.message : '附件读取失败，请先打开邮件正文', 'error')
    } finally {
      attachmentCancelsRef.current.delete(attachment.id)
      setAttachmentProgress((current) => {
        const next = { ...current }
        delete next[attachment.id]
        return next
      })
    }
  }

  const selectFolder = (folder: FolderId) => {
    setSelectedFolder(folder)
    setMobilePane('list')
    setMobileSidebarOpen(false)
    setSelectedNativeFolder(null)
    setSelectedCategory(null)
    setSelectedAccountId(null)
    setSelectedMailIds([])
    if (folder === 'outbox' || folder === 'snoozed') setFilterUnread(false)
    const first = allMails.find((mail) => {
      if (folder === 'starred') return mail.starred
      if (folder === 'snoozed') return Boolean(mail.snoozedUntil)
      if (folder === 'outbox') return mail.folder === 'outbox'
      const account = accountsById.get(mail.accountId)
      return mail.nativeFolder && account
        ? isSameNativeFolder(mail.nativeFolder, nativeFolderName(account, folder))
        : mail.folder === folder
    })
    setSelectedMailId(first?.id ?? '')
    if (folder === 'outbox') {
      void refreshOutbox()
      return
    }
    if (folder === 'snoozed') {
      void refreshSnoozed()
      return
    }
    if (isNativeRuntime && folder !== 'starred') {
      setMailboxHydrating(true)
      void mapWithConcurrency(accounts, ACCOUNT_IPC_CONCURRENCY, async (account) => {
        try {
          const serverFolder = nativeFolderName(account, folder)
          const result = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id, folder: serverFolder, limit: INITIAL_MAILBOX_PAGE_SIZE })
          const converted = (result.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, account))
          rememberMailboxPage(result)
          startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, serverFolder, 'replace')))
        } catch {
          // A provider may not expose every optional folder; its cached copy remains untouched.
        }
      }).finally(() => setMailboxHydrating(false))
    }
  }

  const selectNativeFolder = (account: MailAccount, folder: string) => {
    setSelectedFolder('archive')
    setMobilePane('list')
    setMobileSidebarOpen(false)
    setSelectedNativeFolder({ accountId: account.id, name: folder })
    setSelectedCategory(null)
    setSelectedAccountId(account.id)
    setSelectedMailIds([])
    const first = allMails.find((mail) => mail.accountId === account.id && mail.nativeFolder && isSameNativeFolder(mail.nativeFolder, folder))
    if (first) setSelectedMailId(first.id)
    if (!isNativeRuntime) return
    setMailboxHydrating(true)
    void invoke<NativeMailboxResponse>('mail.list', { accountId: account.id, folder, limit: INITIAL_MAILBOX_PAGE_SIZE }).then((result) => {
      if (!result.mailbox) return
      const converted = result.mailbox.messages.map((message) => nativeMessageToUi(message, account))
      rememberMailboxPage(result)
      startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, folder, 'replace')))
      if (converted.length) setSelectedMailId((current) => !current || current === 'launch-plan' ? converted[0].id : current)
    }).catch(() => pushToast(`${account.label} 的“${nativeFolderLabel(folder, nativeFolderLabels[account.id]?.[folder])}”暂时无法读取`, 'error'))
      .finally(() => setMailboxHydrating(false))
  }

  const loadEarlier = async () => {
    if (!isNativeRuntime || selectedFolder === 'starred' || selectedFolder === 'snoozed' || selectedFolder === 'outbox' || isLoadingEarlier) return
    const targetAccounts = selectedNativeFolder
      ? accounts.filter((account) => account.id === selectedNativeFolder.accountId)
      : accounts.filter((account) => !selectedAccountId || account.id === selectedAccountId)
    const pendingAccounts = targetAccounts.filter((account) => {
      const serverFolder = selectedNativeFolder?.name ?? nativeFolderName(account, selectedFolder)
      return mailboxMeta[nativeMailboxKey(account.id, serverFolder)]?.hasMore
    })
    if (!pendingAccounts.length) {
      pushToast('已经加载到更早的邮件', 'info')
      return
    }
    setLoadingEarlier(true)
    try {
      let loaded = 0
      await mapWithConcurrency(pendingAccounts, ACCOUNT_IPC_CONCURRENCY, async (account) => {
        const serverFolder = selectedNativeFolder?.name ?? nativeFolderName(account, selectedFolder)
        const meta = mailboxMeta[nativeMailboxKey(account.id, serverFolder)]
        try {
          const localPage = await invoke<NativeMailboxResponse>('mail.list', {
            accountId: account.id,
            folder: serverFolder,
            ...(meta?.oldestUid != null ? { beforeUid: meta.oldestUid } : {}),
            limit: EARLIER_MAILBOX_PAGE_SIZE,
          })
          if (localPage.mailbox?.messages.length) {
            const converted = localPage.mailbox.messages.map((message) => nativeMessageToUi(message, account))
            loaded += converted.length
            rememberMailboxPage(localPage)
            startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, serverFolder, 'append')))
            return
          }
          if (offlineMode || !localPage.remoteHasMore) {
            rememberMailboxPage({ ...localPage, localHasMore: false, remoteHasMore: false })
            return
          }
          await invoke<NativeSyncItem>('sync.page', {
            accountId: account.id,
            folder: serverFolder,
            ...(meta?.oldestUid != null ? { beforeUid: meta.oldestUid } : {}),
            limit: EARLIER_MAILBOX_PAGE_SIZE,
          }, 60_000)
          const remotePage = await invoke<NativeMailboxResponse>('mail.list', {
            accountId: account.id,
            folder: serverFolder,
            ...(meta?.oldestUid != null ? { beforeUid: meta.oldestUid } : {}),
            limit: EARLIER_MAILBOX_PAGE_SIZE,
          })
          if (!remotePage.mailbox) return
          const converted = remotePage.mailbox.messages.map((message) => nativeMessageToUi(message, account))
          loaded += converted.length
          rememberMailboxPage(remotePage)
          startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, serverFolder, 'append')))
        } catch {
          pushToast(`${account.label} 的更早邮件加载失败，可稍后重试`, 'error')
        }
      })
      if (loaded) pushToast(`已更新 ${loaded} 封本地邮件`, 'success')
    } finally {
      setLoadingEarlier(false)
    }
  }

  const handleMailListScroll = (event: React.UIEvent<HTMLDivElement>) => {
    const element = event.currentTarget
    const distanceFromBottom = element.scrollHeight - element.scrollTop - element.clientHeight
    if (distanceFromBottom <= 280 && canLoadEarlier && !isLoadingEarlier) {
      void loadEarlier()
    }
  }

  const selectCategory = (category: SmartCategory) => {
    setSelectedCategory(category)
    setMobilePane('list')
    setMobileSidebarOpen(false)
    setSelectedFolder('inbox')
    setSelectedNativeFolder(null)
    setSelectedAccountId(null)
    const first = allMails.find((mail) => category === 'ads'
      ? mail.category === 'ads' || mail.category === 'apple-ads'
      : mail.category === category)
    if (first) setSelectedMailId(first.id)
  }

  const handleSync = async () => {
    if (offlineMode) {
      pushToast('仅离线模式已开启，当前使用本地缓存；关闭后可同步', 'info')
      return
    }
    if (typeof navigator !== 'undefined' && !navigator.onLine) {
      pushToast('当前处于离线模式，已保留本地缓存；联网后可重试同步', 'info')
      return
    }
    setSyncing(true)
    try {
      const result = await invoke<NativeSyncResponse>('sync.all', {}, 60_000)
      setNativeFolders((current) => {
        const next = { ...current }
        for (const item of result.synced ?? []) {
          if (item.folders?.length) next[item.accountId] = item.folders
        }
        return next
      })
      setNativeFolderLabels((current) => {
        const next = { ...current }
        for (const item of result.synced ?? []) {
          if (item.folderLabels) next[item.accountId] = item.folderLabels
        }
        return next
      })
      setAccounts((current) => current.map((account) => {
        const synced = result.synced?.find((item) => item.accountId === account.id)
        const failed = result.failed?.find((item) => item.accountId === account.id)
        if (synced) return { ...account, unread: synced.unread, status: 'synced' as const, lastSync: '刚刚同步' }
        if (failed && /already in progress/i.test(failed.message)) return { ...account, status: 'syncing' as const, lastSync: '后台同步中…' }
        if (failed) {
          const needsAuth = /auth|credential|login|password|authorization/i.test(failed.message)
          return { ...account, status: needsAuth ? 'needs-auth' as const : 'offline' as const, lastSync: needsAuth ? '等待重新授权' : '同步失败，可重试' }
        }
        return account
      }))
      if (result.failed?.length) pushToast(`${result.synced?.length ?? 0} 个账户已同步，${result.failed.length} 个需要处理`, 'info')
      else pushToast('所有账户已完成同步', 'success')
      if (isNativeRuntime) {
        await mapWithConcurrency(accounts, ACCOUNT_IPC_CONCURRENCY, async (account) => {
          try {
            const mailbox = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id, limit: INITIAL_MAILBOX_PAGE_SIZE })
            const converted = (mailbox.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, account))
            if (mailbox.mailbox) {
              rememberMailboxPage(mailbox, true)
              startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, mailbox.mailbox!.folder, 'latest')))
            }
          } catch {
            // Preserve the previous offline copy if one account has a transient cache error.
          }
        })
        await refreshPendingOperations(accounts)
        await refreshOutbox(accounts)
      }
    } catch {
      if (!isNativeRuntime) {
        // Browser preview has no account transport. Keep the design-time demo responsive.
        await new Promise((resolve) => window.setTimeout(resolve, 700))
        setAccounts((current) => current.map((account) => ({ ...account, status: 'synced', lastSync: '刚刚同步' })))
        pushToast('所有账户已完成同步', 'success')
      } else {
        pushToast('同步请求失败，请检查 native shell 或稍后重试', 'error')
      }
    } finally {
      setSyncing(false)
    }
  }

  const refreshAccountMailbox = async (account: MailAccount) => {
    if (!isNativeRuntime) return
    const mailbox = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id, limit: INITIAL_MAILBOX_PAGE_SIZE })
    const converted = (mailbox.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, account))
    if (mailbox.mailbox) {
      rememberMailboxPage(mailbox, true)
      startTransition(() => setMails((current) => mergeMailboxPage(current, converted, account.id, mailbox.mailbox!.folder, 'latest')))
    }
    if (converted.length) setSelectedMailId((current) => current === '' || current === 'launch-plan' ? converted[0].id : current)
  }

  const syncAccountInBackground = async (account: MailAccount, accountList: MailAccount[]) => {
    if (!isNativeRuntime || offlineMode) return
    try {
      const result = await invoke<NativeSyncItem>('sync.account', { accountId: account.id }, 60_000)
      if (result.folders?.length) setNativeFolders((current) => ({ ...current, [account.id]: result.folders! }))
      if (result.folderLabels) setNativeFolderLabels((current) => ({ ...current, [account.id]: result.folderLabels! }))
      setAccounts((current) => current.map((item) => item.id === account.id
        ? { ...item, unread: result.unread ?? 0, status: 'synced', lastSync: '刚刚同步' }
        : item))
      try {
        await refreshAccountMailbox(account)
      } catch {
        // A cache read failure must not downgrade a successful remote sync.
      }
      await Promise.all([
        refreshPendingOperations(accountList),
        refreshOutbox(accountList),
      ])
      pushToast(`${account.label} 已完成首次同步`, 'success')
    } catch (error) {
      const message = error instanceof Error ? error.message : ''
      if (/already in progress/i.test(message)) {
        pushToast(`${account.label} 正在同步`, 'info')
        return
      }
      const needsAuth = /auth|credential|login|password|authorization/i.test(message)
      setAccounts((current) => current.map((item) => item.id === account.id
        ? { ...item, status: needsAuth ? 'needs-auth' as const : 'offline' as const, lastSync: needsAuth ? '等待重新授权' : '首次同步失败，可重试' }
        : item))
      pushToast(needsAuth ? `${account.label} 需要重新授权` : `${account.label} 首次同步失败，可稍后重试`, needsAuth ? 'error' : 'info')
    }
  }

  const editQueuedMessage = async (item: NativeOutboxItem) => {
    if (outboxAction) return
    setOutboxAction({ id: item.id, kind: 'edit' })
    try {
      if (!isNativeRuntime) {
        pushToast('演示数据不会写入本机草稿；安装版会立即打开完整草稿', 'info')
        return
      }
      const result = await invoke<NativeOutboxRecallResponse>('mail.outbox.recall', { accountId: item.accountId, outboxId: item.id }, 30_000)
      if (result.status === 'too-late') {
        pushToast('邮件已经进入发送阶段，无法继续编辑', 'info')
        void refreshOutbox()
        return
      }
      if (result.status === 'missing' || !result.draft) {
        pushToast('这封邮件已不在发件箱，可能已经发送', 'info')
        void refreshOutbox()
        return
      }
      handleDraftChanged(result.draft)
      setSelectedMailId('')
      setSelectedFolder('drafts')
      openCompose(result.draft.id)
      void Promise.all([refreshOutbox(), refreshNativeDrafts()])
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '暂时无法恢复这封待发送邮件', 'error')
    } finally {
      setOutboxAction((current) => current?.id === item.id && current.kind === 'edit' ? null : current)
    }
  }

  const retryQueuedMessage = async (item: NativeOutboxItem) => {
    if (outboxAction) return
    if (offlineMode) {
      pushToast('仅离线模式已开启，关闭后即可继续发送', 'info')
      return
    }
    setOutboxAction({ id: item.id, kind: 'retry' })
    try {
      if (!isNativeRuntime) {
        setNativeOutboxItems((current) => current.map((candidate) => candidate.id === item.id
          ? { ...candidate, state: 'pending' as const, attempts: 0, lastError: undefined, nextAttemptAt: Math.floor(Date.now() / 1_000), scheduledAt: undefined }
          : candidate))
        pushToast('已进入后台发送队列', 'success')
        return
      }
      const result = await invoke<NativeOutboxActionResponse>('mail.outbox.retry', { accountId: item.accountId, outboxId: item.id })
      if (result.status === 'retried') pushToast('已恢复后台发送，无需停留在此页面', 'success')
      else if (result.status === 'too-late') pushToast('邮件已经进入发送阶段', 'info')
      else pushToast('这封邮件已不在发件箱，可能已经发送', 'info')
      await refreshOutbox()
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '暂时无法重试这封邮件', 'error')
    } finally {
      setOutboxAction((current) => current?.id === item.id && current.kind === 'retry' ? null : current)
    }
  }

  const discardQueuedMessage = async () => {
    const item = pendingOutboxDiscard
    if (!item || outboxAction) return
    setOutboxAction({ id: item.id, kind: 'discard' })
    try {
      if (!isNativeRuntime) {
        setNativeOutboxItems((current) => current.filter((candidate) => candidate.id !== item.id))
        setSelectedMailId('')
        setPendingOutboxDiscard(null)
        pushToast('已删除待发送邮件', 'success')
        return
      }
      const result = await invoke<NativeOutboxActionResponse>('mail.outbox.discard', { accountId: item.accountId, outboxId: item.id }, 30_000)
      setPendingOutboxDiscard(null)
      if (result.status === 'discarded') {
        setSelectedMailId('')
        pushToast('已删除待发送邮件及其本地草稿', 'success')
      } else if (result.status === 'too-late') {
        pushToast('邮件已经进入发送阶段，无法删除', 'info')
      } else {
        pushToast('这封邮件已不在发件箱，可能已经发送', 'info')
      }
      await Promise.all([refreshOutbox(), refreshNativeDrafts()])
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '暂时无法删除这封待发送邮件', 'error')
    } finally {
      setOutboxAction((current) => current?.id === item.id && current.kind === 'discard' ? null : current)
    }
  }

  const retryPendingOutbox = async () => {
    if (!accounts.length) return
    if (!isNativeRuntime) {
      const retryable = nativeOutboxItems.filter((item) => item.state === 'paused' || item.state === 'retrying').length
      setNativeOutboxItems((current) => current.map((item) => item.state === 'paused' || item.state === 'retrying'
        ? { ...item, state: 'pending' as const, attempts: 0, lastError: undefined, nextAttemptAt: Math.floor(Date.now() / 1_000) }
        : item))
      pushToast(retryable ? `已恢复 ${retryable} 封待发送邮件` : '当前没有需要手动重试的待发送邮件', 'info')
      return
    }
    if (offlineMode) {
      pushToast('仅离线模式已开启，联网后关闭该模式即可重试发件箱', 'info')
      return
    }
    try {
      const results = await mapWithConcurrency(accounts, ACCOUNT_IPC_CONCURRENCY, (account) => invoke<{ reset: number }>('mail.outbox.retry_all', { accountId: account.id }))
      const reset = results.reduce((total, result) => total + result.reset, 0)
      await refreshOutbox()
      if (reset > 0) {
        pushToast(`已恢复 ${reset} 封待发送邮件，正在尝试发送`, 'info')
        void handleSync()
      } else {
        pushToast('当前没有需要手动重试的待发送邮件', 'info')
      }
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '发件箱暂时无法操作', 'error')
    }
  }

  const handleUndoSend = async (toast: Toast) => {
    const action = toast.action
    if (!action || action.kind !== 'undo-send') return
    dismissToast(toast.id)
    try {
      const result = isNativeRuntime
        ? await invoke<NativeUndoSendResponse>('mail.outbox.undo', {
            accountId: action.accountId,
            outboxId: action.outboxId,
          })
        : { accountId: action.accountId, outboxId: action.outboxId, status: 'cancelled' as const }
      void refreshOutbox()
      if (result.status !== 'cancelled') {
        pushToast('邮件已进入发送阶段，无法撤销', 'info')
        return
      }
      pushToast('已撤销发送，邮件仍保留为草稿', 'success')
      if (action.draftId) {
        void refreshNativeDrafts()
        openCompose(action.draftId)
      }
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '撤销发送失败，请检查发件箱状态', 'error')
    }
  }

  const handleComposeSent = (result: NativeSendResponse) => {
    setComposeOpen(false)
    setComposeDraftId(undefined)
    setComposeMode('new')
    setComposeSource(undefined)
    void refreshNativeDrafts()
    void refreshOutbox()
    if (result.scheduled && result.scheduledFor) {
      pushToast(`已安排在 ${formatScheduledAt(result.scheduledFor)} 发送`, 'success')
      return
    }
    if (result.undoable && result.outboxId) {
      const fallbackDuration = Math.max(1, result.undoSeconds ?? undoSendSeconds) * 1_000
      const durationMs = result.undoExpiresAt
        ? Math.max(1_200, result.undoExpiresAt - Date.now())
        : fallbackDuration
      pushToast(`邮件将在 ${result.undoSeconds ?? undoSendSeconds} 秒后发送`, 'success', {
        durationMs,
        onExpire: () => { void refreshOutbox() },
        action: {
          kind: 'undo-send',
          accountId: result.accountId,
          outboxId: result.outboxId,
          draftId: result.draftId,
        },
      })
      return
    }
    pushToast(result.queued
      ? result.offline
        ? '邮件已加入发件箱，恢复在线后自动发送'
        : '网络暂不可用，邮件已加入发件箱，联网后自动重试'
      : '邮件已发送', 'success')
  }

  const handleUndoSendSecondsChange = (seconds: UndoSendSeconds) => {
    const previous = undoSendSeconds
    setUndoSendSeconds(seconds)
    if (!isNativeRuntime) return
    void invoke('app.set_undo_send_seconds', { seconds }).catch((error) => {
      setUndoSendSeconds(previous)
      pushToast(error instanceof Error ? error.message : '无法保存撤销发送设置', 'error')
    })
  }

  const handleOpenProvider = async () => {
    if (isNativeRuntime && customAuthentication === 'oauth2' && accountEmail.trim()) {
      const attempt = invalidateAuthFlow()
      setOauthSessionId('')
      setOauthState('')
      setDeviceFlow(null)
      try {
        if (provider === 'outlook') {
          const flow = await invoke<NativeDeviceStartResponse>('auth.device.start', {
            provider,
            email: accountEmail.trim(),
          })
          if (attempt !== authAttemptRef.current) {
            cancelNativeAuthSession(flow.sessionId)
            return
          }
          oauthSessionIdRef.current = flow.sessionId
          setOauthSessionId(flow.sessionId)
          setDeviceFlow({
            sessionId: flow.sessionId,
            userCode: flow.userCode,
            verificationUri: flow.verificationUri,
            message: flow.message,
            retryAfter: flow.interval,
            status: 'pending',
          })
          await openExternalUrl(flow.verificationUri)
          pushToast(`Outlook 设备码 ${flow.userCode} 已生成，完成验证后会自动检测`, 'info')
          return
        }
        const flow = await invoke<NativeAuthStartResponse>('auth.start', {
          provider,
          email: accountEmail.trim(),
        })
        if (attempt !== authAttemptRef.current) {
          cancelNativeAuthSession(flow.sessionId)
          return
        }
        oauthSessionIdRef.current = flow.sessionId
        setOauthSessionId(flow.sessionId)
        setOauthState(flow.state)
        await openExternalUrl(flow.authorizationUrl)
        pushToast('OAuth 授权页面已打开，完成后返回 MailGo 点击“开始同步”；回调不可用时再手动粘贴授权码', 'info')
        return
      } catch (error) {
        if (attempt !== authAttemptRef.current) return
        invalidateAuthFlow()
        setOauthSessionId('')
        setOauthState('')
        setDeviceFlow(null)
        pushToast(error instanceof Error ? error.message : 'OAuth 客户端尚未配置，将打开帮助页面', 'error')
      }
    }
    await openExternalUrl(selectedProvider.authUrl)
    pushToast(`${selectedProvider.label}设置已在浏览器中打开`, 'info')
  }

  const handleCopy = async () => {
    if (!authorizationCode) {
      pushToast('请先输入授权码', 'error')
      return
    }
    try {
      await navigator.clipboard.writeText(authorizationCode)
      pushToast('授权码已复制到剪贴板', 'success')
    } catch {
      pushToast('当前环境不允许访问剪贴板', 'error')
    }
  }

  const handleAddAccount = async () => {
    if (isAddingAccountRef.current) return
    if (!accountEmail.includes('@')) {
      pushToast('请输入有效的邮箱地址', 'error')
      return
    }
    if (!authorizationCode.trim() && (customAuthentication !== 'oauth2' || !oauthSessionId)) {
      pushToast('请输入授权码，凭据只会交给本地安全存储', 'error')
      return
    }
    if (customAuthentication === 'oauth2' && deviceFlow && deviceFlow.status !== 'complete' && !authorizationCode.trim()) {
      pushToast('请先完成 Outlook 设备验证，完成后再开始同步', 'info')
      return
    }
    const existingAccount = editingAccountId ? accountsById.get(editingAccountId) : undefined
    const changedMailboxIdentity = existingAccount && (
      existingAccount.provider !== provider
      || existingAccount.email.toLowerCase() !== accountEmail.trim().toLowerCase()
      || (provider === 'other' && (
        existingAccount.imapHost?.toLowerCase() !== customImapHost.trim().toLowerCase()
        || existingAccount.imapPort !== Number(customImapPort)
        || existingAccount.imapSecurity?.toLowerCase() !== customImapSecurity.trim().toLowerCase()
        || existingAccount.smtpHost?.toLowerCase() !== customSmtpHost.trim().toLowerCase()
        || existingAccount.smtpPort !== Number(customSmtpPort)
        || existingAccount.smtpSecurity?.toLowerCase() !== customSmtpSecurity.trim().toLowerCase()
      ))
    )
    if (changedMailboxIdentity) {
      pushToast('账户身份已锁定；如需更换邮箱或服务器，请移除后重新添加', 'error')
      return
    }
    const id = editingAccountId ?? createAccountId(provider)
    const newAccount: MailAccount = {
      id,
      provider,
      label: selectedProvider.label,
      email: accountEmail.trim(),
      unread: 0,
      accent: selectedProvider.accent,
      status: 'syncing',
      lastSync: '后台同步中…',
      authentication: customAuthentication,
      signature: existingAccount?.signature ?? '',
      ...(provider === 'other' ? {
        imapHost: customImapHost.trim(),
        imapPort: Number(customImapPort),
        imapSecurity: customImapSecurity,
        smtpHost: customSmtpHost.trim(),
        smtpPort: Number(customSmtpPort),
        smtpSecurity: customSmtpSecurity,
      } : {}),
    }
    if (!existingAccount && accounts.some((account) => sameMailboxIdentity(account, newAccount))) {
      pushToast('该邮箱已经添加，请直接使用现有账户', 'info')
      return
    }
    isAddingAccountRef.current = true
    setAddingAccount(true)
    setAccounts((current) => existingAccount
      ? current.map((account) => account.id === id ? newAccount : account)
      : [...current, newAccount])
    if (existingAccount) {
      setNativeFolders((current) => {
        const next = { ...current }
        delete next[id]
        return next
      })
      setNativeFolderLabels((current) => {
        const next = { ...current }
        delete next[id]
        return next
      })
    }
    try {
      await invoke('accounts.add', {
        id,
        provider,
        label: selectedProvider.label,
        email: accountEmail.trim(),
        authorizationCode,
        authentication: customAuthentication,
        signature: existingAccount?.signature ?? '',
        ...(oauthSessionId ? { oauthSessionId } : {}),
        ...(oauthSessionId && oauthState ? { oauthState } : {}),
        ...(provider === 'other' ? {
          imapHost: customImapHost.trim(),
          imapPort: Number(customImapPort),
          imapSecurity: customImapSecurity,
          smtpHost: customSmtpHost.trim(),
          smtpPort: Number(customSmtpPort),
          smtpSecurity: customSmtpSecurity,
          authentication: customAuthentication,
        } : {}),
      })
    } catch (error) {
      setAccounts((current) => existingAccount
        ? current.map((account) => account.id === existingAccount.id ? existingAccount : account)
        : current.filter((account) => account.id !== id))
      const message = error instanceof Error ? error.message : ''
      const isDuplicateMailbox = !existingAccount && /already configured/i.test(message)
      const errorMessage = existingAccount
        ? '账户重新授权失败，已保留原账户配置'
        : isDuplicateMailbox
          ? '该邮箱已经添加，请直接使用现有账户'
          : '账户添加失败，请检查授权码、OAuth 配置或服务器设置'
      pushToast(errorMessage, isDuplicateMailbox ? 'info' : 'error')
      return
    } finally {
      isAddingAccountRef.current = false
      setAddingAccount(false)
    }
    setConnectionDiagnostics((current) => Object.fromEntries(Object.entries(current).filter(([accountId]) => accountId !== id)))
    setAccountEmail('')
    setEditingAccountId(null)
    closeAccountModal()
    setSelectedAccountId(id)
    const nextAccounts = existingAccount
      ? accounts.map((account) => account.id === id ? newAccount : account)
      : [...accounts, newAccount]
    if (offlineMode) {
      setAccounts((current) => current.map((account) => account.id === id ? { ...account, unread: 0, status: 'offline', lastSync: '仅离线模式' } : account))
      pushToast(`${selectedProvider.label}账户${existingAccount ? '已重新授权' : '已加入'}，当前为离线模式`, 'success')
      return
    }
    pushToast(`${selectedProvider.label}账户${existingAccount ? '已重新授权' : '已加入'}，后台同步中`, 'success')
    void syncAccountInBackground(newAccount, nextAccounts)
  }

  const handleDiagnoseAccount = async () => {
    const accountId = editingAccountId
    if (!accountId || !isNativeRuntime) return
    if (offlineMode) {
      pushToast('请先关闭仅离线模式，再检测收发连接', 'info')
      return
    }
    setConnectionDiagnostics((current) => ({ ...current, [accountId]: { phase: 'checking' } }))
    try {
      const result = await invoke<NativeConnectionDiagnostic>('accounts.diagnose', { accountId }, 75_000)
      if (result.accountId !== accountId) throw new Error('connection diagnostic account mismatch')
      setConnectionDiagnostics((current) => ({ ...current, [accountId]: { phase: 'ready', result } }))
      pushToast(result.ok ? '收件与发件连接均正常' : '连接检测完成，请查看需要处理的通道', result.ok ? 'success' : 'info')
    } catch (error) {
      const message = connectionDiagnosticError(error)
      setConnectionDiagnostics((current) => ({ ...current, [accountId]: { phase: 'error', message } }))
      markAccountNeedsReauth(accountId, error)
      pushToast(message, /授权/.test(message) ? 'error' : 'info')
    }
  }

  const handleRemoveAccount = async () => {
    if (!editingAccountId) return
    const account = editingAccountId ? accountsById.get(editingAccountId) : undefined
    if (!account || !window.confirm(`确定移除 ${account.label}（${account.email}）吗？本机凭据与缓存也会删除。`)) return
    try {
      if (isNativeRuntime) await invoke('accounts.remove', { id: account.id })
      setAccounts((current) => current.filter((item) => item.id !== account.id))
      setMails((current) => current.filter((mail) => mail.accountId !== account.id))
      setSnoozedMails((current) => current.filter((mail) => mail.accountId !== account.id))
      setNativeDrafts((current) => current.filter((draft) => draft.accountId !== account.id))
      setNativeFolders((current) => {
        const next = { ...current }
        delete next[account.id]
        return next
      })
      setNativeFolderLabels((current) => {
        const next = { ...current }
        delete next[account.id]
        return next
      })
      setSelectedNativeFolder((current) => current?.accountId === account.id ? null : current)
      setMailboxMeta((current) => Object.fromEntries(Object.entries(current).filter(([key]) => !key.startsWith(`${account.id}::`))))
      setConnectionDiagnostics((current) => Object.fromEntries(Object.entries(current).filter(([accountId]) => accountId !== account.id)))
      setSelectedAccountId((current) => current === account.id ? null : current)
      applyRuleSnapshot(mailRules.filter((rule) => rule.accountId !== account.id))
      setEditingAccountId(null)
      closeAccountModal()
      pushToast(`${account.label} 已移除，本机凭据与缓存已清理`, 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '账户移除失败，请稍后重试', 'error')
    }
  }

  const exportAccounts = () => {
    const payload = {
      schemaVersion: 2,
      exportedAt: new Date().toISOString(),
      product: 'MailGo',
      warning: '出于安全原因，授权码不会导出。导入后请为每个账户重新授权。',
      accounts: accounts.map((account) => ({
        id: account.id,
        provider: account.provider,
        label: account.label,
        email: account.email,
        imapHost: account.imapHost,
        imapPort: account.imapPort,
        imapSecurity: account.imapSecurity,
        smtpHost: account.smtpHost,
        smtpPort: account.smtpPort,
        smtpSecurity: account.smtpSecurity,
        authentication: account.authentication,
        signature: account.signature,
        status: 'requires-reauth' as const,
        secretRef: `mailgo://${account.id}`,
      })),
    }
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `mailgo-accounts-${new Date().toISOString().slice(0, 10)}.json`
    anchor.click()
    URL.revokeObjectURL(url)
    pushToast('账户配置已导出（不含授权码）', 'success')
  }

  const importAccounts = async (event: React.ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0]
    event.target.value = ''
    if (!file) return
    if (file.size > 2 * 1024 * 1024) {
      pushToast('配置文件必须小于 2 MB', 'error')
      return
    }
    setImporting(true)
    try {
      const parsed = JSON.parse(await file.text()) as { accounts?: unknown[]; schemaVersion?: number }
      if (![1, 2].includes(parsed.schemaVersion ?? -1) || !Array.isArray(parsed.accounts)) throw new Error('不支持的配置格式')
      const imported = parsed.accounts.flatMap((candidate) => {
        if (!candidate || typeof candidate !== 'object') return []
        const account = candidate as Partial<MailAccount>
        if (typeof account.id !== 'string' || typeof account.email !== 'string' || !isSupportedProvider(account.provider)) return []
        const id = account.id.trim()
        const email = account.email.trim()
        if (!id || id.length > 128 || !email.includes('@') || email.length > 320) return []
        let signature = ''
        try {
          signature = normalizeAccountSignature(typeof account.signature === 'string' ? account.signature : '')
        } catch {
          return []
        }
        return [{
          id,
          provider: account.provider,
          label: typeof account.label === 'string' && account.label.trim() ? account.label.trim().slice(0, 128) : providerFor(account.provider).label,
          email,
          unread: 0,
          accent: typeof account.accent === 'string' ? account.accent : providerFor(account.provider).accent,
          status: 'needs-auth' as const,
          lastSync: '等待重新授权',
          imapHost: typeof account.imapHost === 'string' ? account.imapHost.trim().slice(0, 512) : undefined,
          imapPort: typeof account.imapPort === 'number' ? account.imapPort : undefined,
          imapSecurity: typeof account.imapSecurity === 'string' ? account.imapSecurity.trim().slice(0, 32) : undefined,
          smtpHost: typeof account.smtpHost === 'string' ? account.smtpHost.trim().slice(0, 512) : undefined,
          smtpPort: typeof account.smtpPort === 'number' ? account.smtpPort : undefined,
          smtpSecurity: typeof account.smtpSecurity === 'string' ? account.smtpSecurity.trim().slice(0, 32) : undefined,
          authentication: typeof account.authentication === 'string' ? account.authentication.trim().slice(0, 32) : undefined,
          signature,
        }]
      }).slice(0, 64)
      if (imported.length === 0) throw new Error('配置文件中没有可导入的有效账户')
      const importedIds = new Set(imported.map((account) => account.id))
      if (isNativeRuntime) {
        const result = await invoke<{ imported: number }>('accounts.import', { accounts: imported })
        if (result.imported === 0) throw new Error('没有账户通过本地校验，配置未导入')
        const nativeState = await readNativeState()
        const nextAccounts = nativeState?.accounts ?? accounts.filter((account) => !importedIds.has(account.id)).concat(imported)
        setAccounts(nextAccounts)
        setNativeFolders(nativeState?.folders ?? {})
        setNativeFolderLabels(nativeState?.folderLabels ?? {})
        setMails((current) => current.filter((mail) => !importedIds.has(mail.accountId)))
        void refreshPendingOperations(nextAccounts)
        void refreshOutbox(nextAccounts)
        void refreshSnoozed(nextAccounts)
        void refreshNativeDrafts(nextAccounts)
        void refreshMailRules()
        pushToast(`已导入 ${result.imported} 个账户，请逐一补充授权码`, 'success')
      } else {
        setAccounts((current) => [...current.filter((account) => !importedIds.has(account.id)), ...imported])
        setNativeFolders((current) => Object.fromEntries(Object.entries(current).filter(([accountId]) => !importedIds.has(accountId))))
        setNativeFolderLabels((current) => Object.fromEntries(Object.entries(current).filter(([accountId]) => !importedIds.has(accountId))))
        setMails((current) => current.filter((mail) => !importedIds.has(mail.accountId)))
        applyRuleSnapshot(mailRules.filter((rule) => !rule.accountId || !importedIds.has(rule.accountId)))
        pushToast(`已导入 ${imported.length} 个账户，请逐一补充授权码`, 'success')
      }
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '配置导入失败', 'error')
    } finally {
      setImporting(false)
    }
  }

  const saveAccountSignature = useCallback(async (accountId: string, value: string) => {
    const signature = normalizeAccountSignature(value)
    const saved = isNativeRuntime
      ? await invoke<{ signature: string }>('accounts.set_signature', { accountId, signature })
      : { signature }
    setAccounts((current) => current.map((account) => account.id === accountId
      ? { ...account, signature: saved.signature }
      : account))
    return saved.signature
  }, [isNativeRuntime])

  const handleCloseWindow = () => {
    if (minimizeToTray) {
      if (isNativeRuntime) {
        void invoke('app.hide_window').then(() => pushToast('MailGo 已隐藏到系统托盘，后台继续同步', 'info')).catch(() => window.__RDESKTOP_WINDOW__?.minimize())
      } else {
        window.__RDESKTOP_WINDOW__?.minimize()
        pushToast('MailGo 已缩小到系统托盘，后台继续同步', 'info')
      }
      return
    }
    window.__RDESKTOP_WINDOW__?.close()
  }

  const nativeFolderGroups = useMemo(
    () => accounts
      .map((account) => ({ account, folders: customNativeFolders(account, nativeFolders[account.id]) }))
      .filter((group) => group.folders.length > 0),
    [accounts, nativeFolders],
  )

  const activeMailboxTitle = selectedNativeFolder
    ? nativeFolderLabel(selectedNativeFolder.name, nativeFolderLabels[selectedNativeFolder.accountId]?.[selectedNativeFolder.name])
    : selectedCategory
      ? (smartCategories.find((category) => category.id === selectedCategory)?.label ?? '智能分类')
      : (displayedFolderLabels.find((folder) => folder.id === selectedFolder)?.label ?? '收件箱')
  const composeAccount = accountsById.get(composeSource?.accountId ?? selectedAccountId ?? accounts[0]?.id ?? '')
  const hasAccountNeedingAuth = accounts.some((account) => account.status === 'needs-auth')
  const hasOfflineAccount = accounts.some((account) => account.status === 'offline')
  const syncStatusLabel = nativeStateError
    ? '本地服务连接失败'
    : offlineMode
      ? '仅离线模式'
      : accounts.length === 0
        ? '等待添加账户'
        : isSyncing || accounts.some((account) => account.status === 'syncing')
          ? '正在后台同步'
          : hasAccountNeedingAuth
            ? '账户需要重新授权'
            : hasOfflineAccount
              ? '后台同步待重试'
              : '所有账户已同步'
  const syncStatusTone = nativeStateError || hasAccountNeedingAuth ? 'is-error' : offlineMode || hasOfflineAccount ? 'is-offline' : ''
  const cacheStatsLabel = !isNativeRuntime
    ? '浏览器预览'
    : cacheStats
      ? `${formatStorageBytes(cacheStats.totalBytes)} · ${cacheStats.fileCount.toLocaleString('zh-CN')} 个文件${cacheStatsState === 'loading' ? ' · 更新中' : cacheStats.truncated || cacheStats.unreadableEntries > 0 ? ' · 部分统计' : ''}`
      : cacheStatsState === 'error'
        ? '暂时无法统计'
        : '正在统计…'
  const cacheMailBytes = cacheStats?.mailBytes ?? 0
  const cacheAttachmentBytes = cacheStats?.attachmentBytes ?? 0
  const cacheOtherBytes = Math.max(0, (cacheStats?.totalBytes ?? 0) - cacheMailBytes - cacheAttachmentBytes)
  const cacheCompositionLabel = cacheStats
    ? `邮件 ${formatStorageBytes(cacheMailBytes)}，附件 ${formatStorageBytes(cacheAttachmentBytes)}，草稿、队列与其他数据 ${formatStorageBytes(cacheOtherBytes)}`
    : '正在异步统计本地缓存组成'
  const toggleNavigation = () => {
    if (isMobileLayout) {
      setMobileSidebarOpen((value) => !value)
      return
    }
    setSidebarCollapsed((value) => !value)
  }

  const searchIsBusy = query !== deferredQuery
    || localSearchState === 'searching'
    || localSearchState === 'indexing'
    || (!offlineMode && serverSearchState === 'searching')
  const searchStatusTone = localSearchState === 'error' && (offlineMode || serverSearchState === 'error')
    ? 'error'
    : searchIsBusy ? 'searching' : 'ready'
  const searchStatusLabel = query !== deferredQuery || localSearchState === 'searching'
    ? '本地搜索中…'
    : localSearchState === 'indexing'
      ? '本地索引加载中…'
      : !offlineMode && serverSearchState === 'searching'
        ? '本地已显示 · 云端加载中…'
        : localSearchState === 'error' && (offlineMode || serverSearchState === 'error')
          ? '搜索失败'
          : serverSearchState === 'error'
            ? '云端失败 · 本地结果'
            : localSearchTruncated || serverSearchTruncated
              ? '已显示部分结果'
              : offlineMode ? '本地结果' : '本地 + 云端'

  return (
    <div className={`app-shell ${isCompactDensity ? 'is-compact-density' : ''}`}>
      <style>{`.reicon { width: 1em; height: 1em; }`}</style>
      <header className="titlebar" data-rdesktop-drag="true" onDoubleClick={() => window.__RDESKTOP_WINDOW__?.maximize()}>
        <div className="titlebar-brand" data-no-drag="true">
          <TooltipButton label={isMobileLayout ? (isMobileSidebarOpen ? '关闭导航' : '打开导航') : isSidebarCollapsed ? '展开导航' : '收起导航'} className="titlebar-menu" ariaExpanded={isMobileLayout ? isMobileSidebarOpen : !isSidebarCollapsed} onClick={toggleNavigation}><Icon name="menu" size={20} /></TooltipButton>
          <BrandMark /><span>MailGo</span>
        </div>
        <div className="titlebar-search" data-no-drag="true" onDoubleClick={(event) => event.stopPropagation()}>
          <div className="search-wrap"><Icon name="search" size={19} /><input id="mail-search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索邮件" aria-label="搜索邮件" /><kbd>Ctrl K</kbd>{query.trim().length >= 2 && isNativeRuntime && <span className={`search-status search-${searchStatusTone}`} aria-live="polite">{searchIsBusy && <span className="loading-spinner loading-spinner-small" aria-hidden="true" />}{searchStatusLabel}</span>}</div>
        </div>
        <div className="titlebar-utilities" data-no-drag="true">
          <button type="button" className={`sync-summary ${syncStatusTone}`} onClick={handleSync} aria-label={`${syncStatusLabel}，点击立即同步`}><span className="sync-summary-dot" />{syncStatusLabel}</button>
          <TooltipButton label="授权码助手" active={isAuthPanelOpen} ariaExpanded={isAuthPanelOpen} onClick={() => { setMobileAuthOpen(true); setAuthPanelOpen(true) }}><Icon name="key" size={18} /></TooltipButton>
          <TooltipButton label="偏好设置" active={isSettingsOpen} ariaExpanded={isSettingsOpen} onClick={() => setSettingsOpen((value) => !value)}><Icon name="settings" size={18} /></TooltipButton>
          <TooltipButton label={theme === 'dark' ? '切换到浅色主题' : '切换到深色主题'} onClick={() => setTheme((value) => value === 'dark' ? 'light' : 'dark')}><Icon name={theme === 'dark' ? 'moon' : 'theme'} size={18} /></TooltipButton>
        </div>
        <div className="window-controls" data-no-drag="true">
          <TooltipButton label="写邮件" className="mobile-only-button" onClick={() => openCompose()}><Icon name="edit" size={17} /></TooltipButton>
          <TooltipButton label="最小化" onClick={() => window.__RDESKTOP_WINDOW__?.minimize()}><span className="window-minimize" /></TooltipButton>
          <TooltipButton label="最大化" onClick={() => window.__RDESKTOP_WINDOW__?.maximize()}><Icon name="maximize" size={16} /></TooltipButton>
          <TooltipButton label={minimizeToTray ? '缩小到托盘' : '关闭 MailGo'} onClick={handleCloseWindow} className="close-button"><Icon name="close" size={17} /></TooltipButton>
        </div>
      </header>

      <div className={`workspace mobile-pane-${mobilePane} ${isSidebarCollapsed ? 'is-sidebar-collapsed' : ''}`}>
        {isMobileSidebarOpen && <button className="mobile-overlay" type="button" aria-label="关闭导航" onClick={() => setMobileSidebarOpen(false)} />}
        <aside className={`sidebar ${isMobileSidebarOpen ? 'is-mobile-open' : ''}`}>
          <div className="sidebar-top">
            <button className="compose-button" type="button" aria-label="写邮件" title="写邮件" onClick={() => openCompose()}><Icon name="edit" size={19} /><span>写邮件</span><span className="compose-shortcut">C</span></button>
            <nav className="folder-nav" aria-label="邮件文件夹">
              {displayedFolderLabels.map((folder) => (
                <button key={folder.id} type="button" aria-label={`${folder.label}${folder.unread > 0 ? `，${folder.unread} 封未读` : ''}`} title={folder.label} className={`nav-row ${selectedFolder === folder.id && !selectedCategory && !selectedNativeFolder ? 'is-selected' : ''}`} onClick={() => selectFolder(folder.id)}>
                  <span className="nav-icon"><Icon name={folder.icon as IconName} size={19} weight={selectedFolder === folder.id ? 'Filled' : 'Outline'} /></span>
                  <span>{folder.label}</span>
                  {folder.unread > 0 && <span className={`nav-count ${selectedFolder === folder.id ? 'nav-count-selected' : ''}`}>{formatCount(folder.unread)}</span>}
                </button>
              ))}
            </nav>
            {isNativeRuntime && nativeFolderGroups.length > 0 && <div className="server-folders" aria-label="服务器文件夹">
              <div className="section-label-row server-folders-heading"><span>服务器文件夹</span><small>已发现</small></div>
              <div className="server-folders-list">
                {nativeFolderGroups.flatMap(({ account, folders }) => folders.map((folder) => {
                  const selected = Boolean(selectedNativeFolder && selectedNativeFolder.accountId === account.id && isSameNativeFolder(selectedNativeFolder.name, folder))
                  const unread = unreadNativeFolderCounts.get(nativeFolderCountKey(account.id, folder)) ?? 0
                  const folderLabel = nativeFolderLabel(folder, nativeFolderLabels[account.id]?.[folder])
                  return <button key={`${account.id}::${folder}`} type="button" aria-label={`${account.label} · ${folderLabel}${unread > 0 ? `，${unread} 封未读` : ''}`} className={`nav-row server-folder-row ${selected ? 'is-selected' : ''}`} onClick={() => selectNativeFolder(account, folder)} title={`${account.label} · ${folderLabel}`}>
                    <span className="nav-icon"><Icon name="folder" size={17} weight={selected ? 'Filled' : 'Outline'} /></span>
                    <span className="server-folder-copy"><span>{folderLabel}</span><small>{account.label}</small></span>
                    {unread > 0 && <span className={`nav-count ${selected ? 'nav-count-selected' : ''}`}>{formatCount(unread)}</span>}
                  </button>
                }))}
              </div>
            </div>}
          </div>

          <div className="sidebar-section smart-section">
            <div className="section-label-row"><span>智能分类</span><TooltipButton label="管理分类" onClick={() => setSettingsOpen(true)}><Icon name="settings" size={15} /></TooltipButton></div>
            <div className="smart-list">
              {smartCategories.map((category) => (
                <button key={category.id} type="button" aria-label={category.label} title={category.label} className={`smart-row ${selectedCategory === category.id ? 'is-selected' : ''}`} onClick={() => selectCategory(category.id)}>
                  <span className="smart-dot" style={{ background: category.color }}><Icon name={category.icon} size={14} /></span><span>{category.label}</span>
                </button>
              ))}
            </div>
          </div>

          <div className="sidebar-section accounts-section">
            <div className="section-label-row"><span>账户</span><TooltipButton label="添加账户" onClick={openNewAccount}><Icon name="add" size={16} /></TooltipButton></div>
            <div className="account-list">
              {accounts.map((account) => {
                const displayedUnread = displayedAccountUnreadCounts.get(account.id) ?? 0
                return <button key={account.id} type="button" aria-label={`${account.label}，${displayedUnread} 封未读`} title={account.label} className={`account-row ${selectedAccountId === account.id && !selectedNativeFolder ? 'is-selected' : ''}`} onClick={() => { setSelectedAccountId(account.id); setSelectedCategory(null); setSelectedNativeFolder(null); setSelectedFolder('inbox'); setSelectedMailIds([]); setMobilePane('list'); setMobileSidebarOpen(false) }}>
                  <ProviderMark provider={account.provider} size="sm" />
                  <span className="account-copy"><strong>{account.label}</strong><small>{account.email}</small></span>
                  {displayedUnread > 0 && <span className="account-count">{displayedUnread}</span>}
                  <span className={`sync-dot sync-${account.status}`} aria-label={account.status === 'synced' ? '已同步' : account.status === 'syncing' ? '同步中' : account.status === 'offline' ? '离线' : '需要授权'} />
                </button>
              })}
            </div>
          </div>

          <div className="storage-bar">
            <div className="storage-meta"><span>本地缓存</span><span aria-live="polite">{cacheStatsLabel}</span></div>
            <div className={`storage-track ${cacheStatsState === 'loading' ? 'is-loading' : ''}`} role="img" aria-label={cacheCompositionLabel} title={cacheCompositionLabel}>
              {cacheStats && cacheStats.totalBytes > 0 ? <><span className="storage-segment storage-mail" style={{ width: storageShare(cacheMailBytes, cacheStats.totalBytes) }} /><span className="storage-segment storage-attachments" style={{ width: storageShare(cacheAttachmentBytes, cacheStats.totalBytes) }} /><span className="storage-segment storage-other" style={{ width: storageShare(cacheOtherBytes, cacheStats.totalBytes) }} /></> : null}
            </div>
            <div className="storage-foot"><span aria-live="polite"><Icon name={outboxTotal || pendingOperations ? 'rotate' : 'cloud'} size={13} /> {outboxTotal ? `${outboxTotal} 封待发送${outboxScheduled ? ` · ${outboxScheduled} 封定时` : ''}${outboxUndoable ? ` · ${outboxUndoable} 封可撤销` : ''}${outboxPaused ? ` · ${outboxPaused} 封需重试` : ''}` : pendingOperations ? `${pendingOperations} 项操作待同步` : '离线可查看最近邮件'}</span><div className="storage-foot-actions">{outboxPaused > 0 && <button type="button" onClick={() => { void retryPendingOutbox() }}><Icon name="rotate" size={13} />重试待发送</button>}<button type="button" onClick={handleSync}><Icon name="rotate" size={13} /> {isSyncing ? '同步中…' : '立即同步'}</button></div></div>
          </div>

          <div className="sidebar-quick-settings">
            <OfflineModeQuickSetting enabled={offlineMode} onToggle={() => setOfflineMode((value) => {
              const next = !value
              pushToast(next ? '仅离线模式已开启，后续邮件操作将保留在本机' : '已恢复在线模式，可重新同步和发送邮件', 'info')
              return next
            })} />
            <button type="button" className={hideAds ? 'is-on' : ''} aria-label={hideAds ? '广告已屏蔽' : '广告已分类'} aria-pressed={hideAds} onClick={() => { const next = !hideAds; setHideAds(next); void invoke('app.set_hide_ads', { enabled: next }).catch(() => undefined) }}>
              <Icon name="shield" size={15} />
              <span>广告 {hideAds ? '已屏蔽' : '已分类'}</span>
              <small>{hideAds ? '普通列表隐藏' : '普通列表显示'}</small>
            </button>
          </div>

          <div className="sidebar-footer">
            <TooltipButton label="设置" active={isSettingsOpen} onClick={() => setSettingsOpen((value) => !value)}><Icon name="settings" size={19} /></TooltipButton>
            <TooltipButton label="帮助中心" active={isHelpOpen} onClick={() => { setOpenMenu(null); setHelpOpen(true) }}><Icon name="help" size={19} /></TooltipButton>
            <TooltipButton label={isSidebarCollapsed ? '展开侧栏' : '收起侧栏'} className="sidebar-collapse" onClick={toggleNavigation}><Icon name="menu" size={19} /></TooltipButton>
          </div>
        </aside>

        <main className="mail-list-panel">
          <div className="panel-toolbar">
            <div className="mailbox-heading"><strong>{activeMailboxTitle}</strong><span>{visibleMails.length} {selectedFolder === 'outbox' ? '封待发送' : selectedFolder === 'snoozed' ? '封稍后处理' : '封邮件'}</span></div>
            {selectedFolder !== 'outbox' && <button className={`filter-button ${filterUnread ? 'is-active' : ''}`} type="button" onClick={() => setFilterUnread((value) => !value)}><Icon name="filter" size={17} /> 筛选{filterUnread && <span className="filter-dot" />}</button>}
          </div>
          {selectedFolder === 'inbox' && !selectedNativeFolder && <nav className="inbox-tabs" aria-label="收件箱分类">
            {inboxTabs.map((tab) => {
              const selected = tab.category === selectedCategory
              return <button key={tab.id} type="button" className={selected ? 'is-selected' : ''} aria-current={selected ? 'page' : undefined} onClick={() => { if (tab.category) selectCategory(tab.category); else selectFolder('inbox') }}><Icon name={tab.icon} size={17} weight={selected ? 'Filled' : 'Outline'} /><span>{tab.label}</span></button>
            })}
          </nav>}
          {selectedFolder === 'outbox' && !selectedNativeFolder ? <div className="list-toolbar outbox-list-toolbar">
            <span><Icon name="cloud" size={15} />本机队列即时显示，发送在后台继续</span>
            {outboxPaused > 0 && <button type="button" className="toolbar-action" onClick={() => { void retryPendingOutbox() }}><Icon name="rotate" size={16} /><span>重试全部</span></button>}
          </div> : <div className="list-toolbar">
            <label className="checkbox-wrap"><input type="checkbox" aria-label="选择所有邮件" checked={allVisibleSelected} onChange={toggleAllVisible} /><span /></label>{selectedVisibleMails.length > 0 && <span className="selection-count">已选 {selectedVisibleMails.length} 封</span>}
            <button type="button" className="toolbar-action" onClick={() => { void applyBulkMove('archive') }} disabled={!selectedVisibleMails.length}><Icon name="archive" size={17} /> <span>归档</span></button>
            <button type="button" className="toolbar-action" onClick={() => { void applyBulkMove('delete') }} disabled={!selectedVisibleMails.length}><Icon name="trash" size={17} /> <span>删除</span></button>
            <button type="button" className="toolbar-action" onClick={() => { void markSelectedRead() }} disabled={!selectedVisibleMails.length}><Icon name="message" size={17} /> <span>标为已读</span></button>
            <div className="menu-anchor">
              <TooltipButton label="更多操作" active={openMenu === 'bulk'} ariaExpanded={openMenu === 'bulk'} onClick={() => setOpenMenu((current) => current === 'bulk' ? null : 'bulk')}><Icon name="more" size={18} /></TooltipButton>
              {openMenu === 'bulk' && <div className="action-menu" role="menu" aria-label="更多批量操作">
                <button type="button" role="menuitem" disabled={!selectedVisibleMails.length} onClick={markSelectedUnread}><Icon name="message" size={16} />标为未读</button>
                <button type="button" role="menuitem" disabled={!selectedVisibleMails.length} onClick={() => { void setSelectedStarred(true) }}><Icon name="star" size={16} />批量加星</button>
                <button type="button" role="menuitem" disabled={!selectedVisibleMails.length} onClick={() => { void setSelectedStarred(false) }}><Icon name="star" size={16} />批量取消星标</button>
                <button type="button" role="menuitem" disabled={!selectedVisibleMails.length} onClick={() => { void applyBulkMove('spam') }}><Icon name="shield" size={16} />移入垃圾邮件</button>
                <button type="button" role="menuitem" disabled={!selectedVisibleMails.length} onClick={() => { void applyBulkMove('inbox') }}><Icon name="inbox" size={16} />移回收件箱</button>
                <button type="button" role="menuitem" disabled={!selectedVisibleMails.length} onClick={() => { setSelectedMailIds([]); setOpenMenu(null) }}><Icon name="close" size={16} />取消选择</button>
              </div>}
            </div>
          </div>}
          <div ref={mailListRef} className="mail-list-scroll" onScroll={handleMailListScroll} aria-busy={isNativeRuntime && (!nativeStateReady || isMailboxHydrating) || isLoadingEarlier || Boolean(outboxAction) || Boolean(snoozeActionId)}>
            {isNativeRuntime && (!nativeStateReady || isMailboxHydrating) && <div className="list-loading-strip" role="status"><span className="loading-spinner" aria-hidden="true" /><span>{nativeStateReady ? '正在从本地索引加载当前页，其他区域仍可操作' : '正在读取本地账户，主界面已可操作'}</span></div>}
            {nativeStateError && <div className="local-state-error" role="alert"><Icon name="info" size={18} /><span><strong>本地邮件服务连接失败</strong>{nativeStateError}<small>账户文件与安全凭据保持原样，请重新启动 MailGo 或查看本机日志。</small></span></div>}
            {virtualMailItems.length > 0 && <div className="mail-virtual-space" style={{ height: mailListVirtualizer.getTotalSize() }}>
              {mailListVirtualizer.getVirtualItems().map((virtualItem) => {
                const item = virtualMailItems[virtualItem.index]
                if (!item) return null
                const latest = item.type === 'thread' ? item.thread.latest : undefined
                return <div key={item.key} className={`virtual-mail-item virtual-mail-${item.type}`} style={{ height: virtualItem.size, transform: `translateY(${virtualItem.start}px)` }}>
                  {item.type === 'group'
                    ? <div className="mail-group-label">{item.label}</div>
                    : <div className={`mail-row ${latest?.outboxId ? 'is-outbox-row' : ''} ${latest?.snoozedUntil ? 'is-snoozed-row' : ''} ${item.thread.messages.some((mail) => mail.id === selectedMailId) ? 'is-selected' : ''} ${item.thread.unreadCount > 0 ? 'is-unread' : ''}`} onClick={() => { void selectMail(item.thread.latest) }}>
                        {latest?.outboxId ? <span className={`outbox-row-state is-${latest.outboxState}`} title={latest.outboxState === 'paused' ? '需要处理' : latest.outboxState === 'scheduled' ? latest.outboxScheduledAt ? '定时发送' : '等待撤销窗口' : latest.outboxState === 'retrying' ? '自动重试中' : '等待发送'}><Icon name={latest.outboxState === 'paused' ? 'info' : latest.outboxState === 'scheduled' ? 'clock' : 'rotate'} size={15} /></span> : <><label className="checkbox-wrap row-checkbox" onClick={(event) => event.stopPropagation()}><input type="checkbox" aria-label={`选择会话 ${latest?.subject}`} checked={item.thread.messages.every((mail) => selectedMailIds.includes(mail.id))} onChange={() => toggleThreadSelection(item.thread)} /><span /></label><button type="button" className={`star-button ${latest?.starred ? 'is-starred' : ''}`} aria-label={latest?.starred ? '取消最新邮件星标' : '为最新邮件添加星标'} onClick={(event) => { event.stopPropagation(); if (latest) toggleStar(latest) }}><Icon name="star" size={18} weight={latest?.starred ? 'Filled' : 'Outline'} /></button></>}
                        <div className="mail-row-copy"><div className="mail-row-top"><strong>{item.thread.participants.join('、') || latest?.senderName}{item.thread.messages.length > 1 && <span className="thread-count">{item.thread.messages.length}</span>}</strong><time>{latest?.timestamp}</time></div><div className="mail-row-subject">{latest?.subject}</div><p>{latest?.preview}</p></div>
                      </div>}
                </div>
              })}
            </div>}
            {visibleMails.length === 0 && <div className="empty-list"><span className="empty-icon"><Icon name={selectedFolder === 'outbox' ? 'send' : selectedFolder === 'snoozed' ? 'clock' : isNativeRuntime && (!nativeStateReady || isMailboxHydrating) ? 'rotate' : accounts.length === 0 ? 'user' : 'search'} size={24} /></span><strong>{selectedFolder === 'outbox' ? '发件箱是空的' : selectedFolder === 'snoozed' ? '没有稍后处理的邮件' : isNativeRuntime && (!nativeStateReady || isMailboxHydrating) ? '正在打开本地邮箱' : accounts.length === 0 ? '还没有添加邮箱账户' : '没有找到邮件'}</strong><p>{selectedFolder === 'outbox' ? '待发送邮件会先安全写入本机，再由后台异步发送。' : selectedFolder === 'snoozed' ? '在阅读工具栏点击时钟，可让邮件在指定时间自动回到收件箱。' : isNativeRuntime && (!nativeStateReady || isMailboxHydrating) ? '先显示本地索引页；远程邮件会在进入后异步同步。' : accounts.length === 0 ? '添加 Google、QQ、Outlook 或自定义 IMAP/SMTP 账户后开始使用。' : '试试清除筛选或搜索其他关键词。'}</p>{isNativeRuntime && nativeStateReady && accounts.length === 0 && <button type="button" className="empty-list-action" onClick={openNewAccount}><Icon name="add" size={16} />添加第一个账户</button>}</div>}
            {isLoadingEarlier && <div className="mail-page-loading" role="status"><span className="loading-spinner" aria-hidden="true" />正在增量加载更早邮件…</div>}
          </div>
          <div className="list-footer"><span>{selectedFolder === 'outbox' ? `${visibleMails.length} 封待发送` : selectedFolder === 'snoozed' ? `${visibleMails.length} 封稍后处理` : visibleThreads.length ? `${visibleThreads.length} 个会话 · ${visibleMails.length} 封邮件` : '0 封邮件'}</span><div className="list-footer-actions">{canLoadEarlier && <button type="button" className="load-earlier-button" onClick={() => { void loadEarlier() }} disabled={isLoadingEarlier}>{isLoadingEarlier ? '加载中…' : '加载更早邮件'}</button>}<TooltipButton label={selectedFolder === 'outbox' ? '刷新发件箱' : selectedFolder === 'snoozed' ? '刷新稍后处理' : '刷新邮件'} onClick={selectedFolder === 'outbox' ? () => { void refreshOutbox() } : selectedFolder === 'snoozed' ? () => { void refreshSnoozed() } : handleSync}><Icon name="rotate" size={17} /></TooltipButton></div></div>
        </main>

        <section className="reading-panel" aria-label="邮件阅读区">
          {selectedOutboxItem ? <OutboxDetail
            item={selectedOutboxItem}
            account={selectedMailAccount}
            busyAction={outboxAction?.id === selectedOutboxItem.id ? outboxAction.kind : undefined}
            onBack={() => setMobilePane('list')}
            onEdit={() => { void editQueuedMessage(selectedOutboxItem) }}
            onRetry={() => { void retryQueuedMessage(selectedOutboxItem) }}
            onDiscard={() => setPendingOutboxDiscard(selectedOutboxItem)}
          /> : selectedMail.id === 'empty-mail' ? <div className="reading-empty-state">
            <TooltipButton label="返回邮件列表" className="mobile-only-button reading-empty-back" onClick={() => setMobilePane('list')}><span className="mobile-back-label">列表</span></TooltipButton>
            <span className="reading-empty-icon"><Icon name={selectedFolder === 'outbox' ? 'send' : selectedFolder === 'snoozed' ? 'clock' : 'inbox'} size={25} /></span>
            <strong>{selectedFolder === 'outbox' ? '发件箱为空' : selectedFolder === 'snoozed' ? '稍后处理为空' : accounts.length === 0 ? '添加邮箱后开始阅读' : '选择一封邮件开始阅读'}</strong>
            <p>{selectedFolder === 'outbox' ? '待发送邮件会安全保存在本机。' : selectedFolder === 'snoozed' ? '到点的邮件会自动回到收件箱。' : accounts.length === 0 ? '支持 Google、QQ、Outlook 和自定义邮箱。' : '邮件列表和正文会按需异步加载。'}</p>
          </div> : <>
          <div className="reading-toolbar">
            <div className="reading-actions"><TooltipButton label="返回邮件列表" className="mobile-only-button reading-back-button" onClick={() => setMobilePane('list')}><span className="mobile-back-label">列表</span></TooltipButton><TooltipButton label="回复" onClick={() => openCompose(undefined, 'reply', selectedMail)}><Icon name="reply" size={18} /></TooltipButton><span>回复</span><TooltipButton label="回复全部" onClick={() => openCompose(undefined, 'reply-all', selectedMail)}><Icon name="reply" size={18} /></TooltipButton><span>回复全部</span><TooltipButton label="转发" onClick={() => openCompose(undefined, 'forward', selectedMail)}><Icon name="forward" size={18} /></TooltipButton><span>转发</span>{selectedMail.snoozedUntil ? <TooltipButton label="取消稍后处理" disabled={snoozeActionId === selectedMail.id} onClick={() => { void unsnoozeMail(selectedMail) }}><Icon name="clock" size={18} weight="Filled" /></TooltipButton> : <SnoozeControl disabled={selectedMail.id === 'empty-mail' || Boolean(selectedMail.outboxId) || snoozeActionId === selectedMail.id} onSnooze={(timestamp) => snoozeMail(selectedMail, timestamp)} />}<TooltipButton label="归档" onClick={() => { void runMove(selectedMail, 'archive') }}><Icon name="archive" size={18} /></TooltipButton><span>归档</span><TooltipButton label="删除" onClick={() => { void runMove(selectedMail, 'delete') }}><Icon name="trash" size={18} /></TooltipButton><span>删除</span><TooltipButton label="移入垃圾邮件" onClick={() => { void runMove(selectedMail, 'spam') }}><Icon name="shield" size={18} /></TooltipButton><span>垃圾邮件</span></div>
            <div className="reading-toolbar-tail">
              <div className="mail-content-scale" role="group" aria-label="邮件正文显示比例">
                <button type="button" aria-label="缩小邮件正文" title="缩小邮件正文" disabled={mailContentScale === MAIL_CONTENT_SCALES[0]} onClick={() => setMailContentScale((current) => MAIL_CONTENT_SCALES[Math.max(0, MAIL_CONTENT_SCALES.indexOf(current) - 1)])}>A−</button>
                <output aria-live="polite" aria-label={`邮件正文 ${mailContentScale}%`}>{mailContentScale}%</output>
                <button type="button" aria-label="放大邮件正文" title="放大邮件正文" disabled={mailContentScale === MAIL_CONTENT_SCALES.at(-1)} onClick={() => setMailContentScale((current) => MAIL_CONTENT_SCALES[Math.min(MAIL_CONTENT_SCALES.length - 1, MAIL_CONTENT_SCALES.indexOf(current) + 1)])}>A+</button>
              </div>
              <div className="menu-anchor">
                <TooltipButton label="更多邮件操作" active={openMenu === 'message'} ariaExpanded={openMenu === 'message'} onClick={() => setOpenMenu((current) => current === 'message' ? null : 'message')}><Icon name="more" size={19} /></TooltipButton>
                {openMenu === 'message' && <div className="action-menu" role="menu" aria-label="更多邮件操作">
                  <button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail'} onClick={() => { void markSelectedMessageUnread() }}><Icon name="message" size={16} />标为未读</button>
                  {selectedMail.snoozedUntil && <button type="button" role="menuitem" disabled={snoozeActionId === selectedMail.id} onClick={() => { setOpenMenu(null); void unsnoozeMail(selectedMail) }}><Icon name="clock" size={16} />取消稍后处理</button>}
                  {selectedMail.folder !== 'inbox' && <button type="button" role="menuitem" onClick={() => { setOpenMenu(null); void runMove(selectedMail, 'inbox') }}><Icon name="inbox" size={16} />移回收件箱</button>}
                  {selectedMailMoveTargets.length > 0 && <div className="action-menu-section"><span className="action-menu-label">移动到</span>{selectedMailMoveTargets.map((target) => <button key={target.folder} type="button" role="menuitem" onClick={() => { setOpenMenu(null); void runMoveToFolder(selectedMail, target) }}><Icon name={target.icon} size={16} />{target.label}</button>)}</div>}
                  <div className="action-menu-section"><span className="action-menu-label">智能屏蔽</span>{selectedMail.blocked ? <button type="button" role="menuitem" onClick={() => { setOpenMenu(null); setMailRuleError(''); setMailRulesOpen(true) }}><Icon name="shieldCheck" size={16} />管理命中的屏蔽规则</button> : <><button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail' || Boolean(mailRuleBusyKey)} onClick={() => { void blockMail(selectedMail, 'sender') }}><Icon name="user" size={16} />屏蔽此发件人</button><button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail' || !domainFromSender(selectedMail.from) || Boolean(mailRuleBusyKey)} onClick={() => { void blockMail(selectedMail, 'domain') }}><Icon name="link" size={16} />屏蔽该发件域名</button></>}</div>
                  <button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail'} onClick={() => { void copySelectedMessage() }}><Icon name="copy" size={16} />复制邮件正文</button>
                  <button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail'} onClick={() => { setOpenMenu(null); window.print() }}><Icon name="document" size={16} />打印邮件</button>
                </div>}
              </div>
            </div>
          </div>
          <div className="reading-scroll">
            <div className="reading-heading"><div><h1>{selectedMail.subject}</h1><div className="message-tags"><span className="tag tag-account"><ProviderMark provider={selectedMailAccount?.provider ?? 'google'} size="sm" /> {selectedMailAccount?.label ?? 'Google'}</span>{selectedThread && selectedThread.messages.length > 1 && <span className="tag"><Icon name="message" size={13} />{selectedThread.messages.length} 封会话</span>}{selectedMail.snoozedUntil && <span className="tag tag-snoozed"><Icon name="clock" size={13} />{formatSnoozeTime(selectedMail.snoozedUntil * 1_000)} 提醒</span>}{selectedMail.blocked && <span className="tag tag-blocked"><Icon name="shieldCheck" size={13} />已按本机规则屏蔽</span>}{selectedMail.hasHtml && <span className="tag">HTML 邮件</span>}</div></div><TooltipButton label={selectedMail.starred ? '取消星标' : '添加星标'} className={`reading-star ${selectedMail.starred ? 'is-starred' : ''}`} onClick={() => toggleStar(selectedMail)}><Icon name="star" size={24} weight={selectedMail.starred ? 'Filled' : 'Outline'} /></TooltipButton></div>
            <ConversationStack thread={selectedThread} selectedId={selectedMail.id} loadingId={loadingMessageId} onSelect={(mail) => { void selectMail(mail) }} />
            <div className="sender-row"><Avatar message={selectedMail} size="lg" /><div className="sender-copy"><div><strong>{selectedMail.senderName}</strong> <span>&lt;{selectedMail.from}&gt;</span></div><div className="recipient">收件人： {selectedMailAccount?.label ?? '当前账户'} &lt;{selectedMailAccount?.email ?? '—'}&gt;</div></div><time>{selectedMail.timestamp}<br /><span>今天</span></time><TooltipButton label="发件人更多信息"><Icon name="more" size={19} /></TooltipButton></div>
            {loadingMessageId === selectedMail.id && <div className="reading-loading-strip" role="status"><span className="loading-spinner" aria-hidden="true" /><span><strong>正在补全邮件正文</strong>列表和已缓存摘要仍可继续浏览</span></div>}
            <div className="message-content" style={{ '--mail-content-font-size': `${((isCompactDensity ? 11.5 : 13) * mailContentScale / 100).toFixed(2)}px` } as React.CSSProperties}>
              {selectedMail.hasHtml && <div className="content-mode-row"><span>此邮件包含富文本内容{(!remoteImagesEnabled || offlineMode) && ` · ${offlineMode ? '仅离线模式，远程图片已屏蔽' : '远程图片已屏蔽'}`}</span><button type="button" className="text-action" onClick={() => setHtmlMode((value) => !value)}>{isHtmlMode ? '查看纯文本' : '渲染 HTML'} <Icon name="grid" size={14} /></button></div>}
              {isHtmlMode && selectedMail.hasHtml ? <div className="html-rendered" onClick={handleRenderedLinkClick} dangerouslySetInnerHTML={{ __html: sanitizeHtml(selectedMail.htmlBody ?? initialHtml, remoteImagesEnabled && !offlineMode) }} /> : selectedMail.body.map((paragraph) => <p key={paragraph}>{paragraph}</p>)}
            </div>
            {selectedMail.attachments && selectedMail.attachments.length > 0 && <div className="attachments"><div className="attachments-heading"><span><Icon name="paperclip" size={20} /> {selectedMail.attachments.length} 个附件</span><div><button type="button" onClick={() => { void mapWithConcurrency(selectedMail.attachments ?? [], ATTACHMENT_DOWNLOAD_CONCURRENCY, downloadAttachment) }}><Icon name="download" size={17} /> 全部下载</button></div></div><div className="attachment-grid">{selectedMail.attachments.map((attachment) => { const progress = attachmentProgress[attachment.id]; return <AttachmentCard attachment={attachment} progress={progress} key={attachment.id} onActivate={() => { if (progress != null) cancelAttachment(attachment.id); else void downloadAttachment(attachment) }} /> })}</div></div>}
            <div className="reply-composer"><Avatar message={{ ...selectedMail, avatar: 'OC', accent: '#2a5596' }} size="sm" /><div className="reply-input" onClick={() => openCompose(undefined, 'reply', selectedMail)}>点击回复，或按 R 快速回复<div className="reply-tools"><span><Icon name="paperclip" size={19} /></span><span><Icon name="image" size={19} /></span><span className="reply-emoji">☺</span><span className="reply-a">A</span><button type="button" onClick={(event) => { event.stopPropagation(); openCompose(undefined, 'reply', selectedMail) }}>回复 <span>⌄</span></button></div></div></div>
          </div>
          </>}
        </section>

        <AnimatePresence initial={false}>
          {isAuthPanelOpen && <motion.aside className={`auth-panel ${isMobileAuthOpen ? 'is-mobile-open' : ''}`} initial={prefersReducedMotion ? false : { x: 24, opacity: 0 }} animate={{ x: 0, opacity: 1 }} exit={{ x: 24, opacity: 0 }} transition={{ duration: 0.24 }}>
            <div className="auth-panel-header"><div><Icon name="key" size={20} /><strong>授权码助手</strong></div><TooltipButton label="关闭授权码助手" onClick={() => { setAuthPanelOpen(false); setMobileAuthOpen(false) }}><Icon name="close" size={18} /></TooltipButton></div>
            <div className="auth-tabs"><button type="button" className="is-active"><Icon name="lock" size={16} />授权码</button><button type="button" onClick={openNewAccount}><Icon name="settings" size={16} />设置</button></div>
            <div className="auth-card"><div className="auth-illustration"><Icon name="shieldCheck" size={40} /></div><h2>快速获取授权码</h2><p>用于第三方服务登录验证</p><button className="gradient-button" type="button" onClick={openNewAccount}><Icon name="copy" size={17} />管理授权码</button><div className="auth-validity"><Icon name="clock" size={16} />授权码仅保存在本机安全存储</div></div>
            <div className="auth-panel-section"><div className="panel-section-title">账户</div>{accounts.map((account) => <button type="button" className="auth-account-row" key={account.id} onClick={() => { setEditingAccountId(account.id); changeProvider(account.provider); setAccountEmail(account.email); setCustomImapHost(account.imapHost ?? 'imap.example.com'); setCustomImapPort(String(account.imapPort ?? 993)); setCustomImapSecurity(account.imapSecurity ?? 'tls'); setCustomSmtpHost(account.smtpHost ?? 'smtp.example.com'); setCustomSmtpPort(String(account.smtpPort ?? 465)); setCustomSmtpSecurity(account.smtpSecurity ?? 'tls'); setCustomAuthentication(account.authentication ?? (account.provider === 'outlook' ? 'oauth2' : 'app-password')); setAccountModalOpen(true) }}><ProviderMark provider={account.provider} size="sm" /><span><strong>{account.label}</strong><small>{account.email}</small></span><span className="auth-chevron">›</span></button>)}</div>
            <div className="auth-note"><Icon name="info" size={18} /><span>授权码仅用于登录验证<br />不会存储或同步到云端</span></div>
            <div className="auth-panel-foot"><button type="button" onClick={handleOpenProvider}><Icon name="link" size={15} />打开 {selectedProvider.label} 设置</button></div>
          </motion.aside>}
        </AnimatePresence>
        {!isAuthPanelOpen && <button className="auth-panel-reopen" type="button" onClick={() => { setAuthPanelOpen(true); setMobileAuthOpen(true) }}><Icon name="key" size={18} />授权码助手</button>}
      </div>

      <AnimatePresence>
        {isSettingsOpen && <motion.div className="settings-popover" initial={{ opacity: 0, y: 8 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0, y: 8 }}>
          <div className="settings-title"><span><Icon name="settings" size={17} />偏好设置</span><TooltipButton label="关闭设置" onClick={() => setSettingsOpen(false)}><Icon name="close" size={17} /></TooltipButton></div>
          <div className="settings-row"><span><Icon name={theme === 'dark' ? 'moon' : 'theme'} size={17} /><span>外观主题<small>{theme === 'dark' ? '深色 · 午夜蓝' : '浅色 · 雪白'}</small></span></span><button type="button" className="theme-switch" onClick={() => setTheme((value) => value === 'dark' ? 'light' : 'dark')}><span className={theme === 'light' ? 'is-light' : ''}>{theme === 'dark' ? '深' : '浅'}</span></button></div>
          <div className="settings-row"><span><Icon name="menu" size={17} /><span>界面密度<small>{displayDensity === 'compact' ? '紧凑 · 高信息密度' : viewportRequiresCompactDensity ? '窗口较小，已自动紧凑' : '舒适 · 更大间距'}</small></span></span><button type="button" aria-label="紧凑桌面布局" className={`toggle-switch ${displayDensity === 'compact' ? 'is-on' : ''}`} onClick={() => setDisplayDensity((value) => value === 'compact' ? 'comfortable' : 'compact')}><span /></button></div>
          <label className="settings-row settings-select-row"><span><Icon name="clock" size={17} /><span>撤销发送<small>{undoSendSeconds === 0 ? '关闭后立即连接邮件服务器发送' : `发送后保留 ${undoSendSeconds} 秒撤销窗口`}</small></span></span><select aria-label="撤销发送等待时间" value={undoSendSeconds} onChange={(event) => handleUndoSendSecondsChange(asUndoSendSeconds(Number(event.target.value)))}><option value={0}>关闭</option><option value={5}>5 秒</option><option value={10}>10 秒</option><option value={20}>20 秒</option><option value={30}>30 秒</option></select></label>
          <label className="settings-row css-row"><span><Icon name="brush" size={17} /><span>用户 CSS<small>{sanitizedCustomCss.removedUnsafeSyntax ? '已过滤外部资源与危险语法' : '可覆盖 MailGo 视觉变量；不加载外部资源'}</small></span></span><textarea value={customCss} onChange={(event) => setCustomCss(event.target.value)} placeholder="例如：:root { --accent: #ff6b8a; }" /></label>
          <AccountSignatureSettings accounts={accounts} initialAccountId={selectedAccountId} onSave={saveAccountSignature} />
          <div className="settings-row"><span><Icon name="shieldCheck" size={17} /><span>屏蔽规则<small>{mailRules.length ? `${mailRules.length} 条 · 加密保存在本机` : '按发件人或域名过滤，不上传云端'}</small></span></span><button type="button" className="settings-text-button" onClick={() => { setSettingsOpen(false); setMailRuleError(''); setMailRulesOpen(true) }}>管理</button></div>
          <div className="settings-row"><span><Icon name="cloud" size={17} /><span>关闭时后台运行<small>最小化到系统托盘并继续同步</small></span></span><button type="button" className={`toggle-switch ${minimizeToTray ? 'is-on' : ''}`} onClick={() => { const next = !minimizeToTray; setMinimizeToTray(next); void invoke('app.set_minimize_to_tray', { enabled: next }).catch(() => undefined) }}><span /></button></div>
          <div className="settings-row"><span><Icon name="image" size={17} /><span>加载远程图片<small>{remoteImagesEnabled ? '已允许 HTTPS 图片，可能包含追踪像素' : '默认屏蔽，保护隐私；CID 内嵌图片不受影响'}</small></span></span><button type="button" aria-label="加载远程图片" className={`toggle-switch ${remoteImagesEnabled ? 'is-on' : ''}`} onClick={() => { const next = !remoteImagesEnabled; setRemoteImagesEnabled(next); void invoke('app.set_remote_images', { enabled: next }).catch(() => undefined) }}><span /></button></div>
          <div className="settings-row"><span><Icon name="bell" size={17} /><span>后台新邮件提醒<small>窗口隐藏时发送 Windows 托盘通知</small></span></span><button type="button" aria-label="后台新邮件提醒" className={`toggle-switch ${notificationsEnabled ? 'is-on' : ''}`} onClick={() => { const next = !notificationsEnabled; setNotificationsEnabled(next); void invoke('app.set_notifications', { enabled: next }).catch(() => undefined) }}><span /></button></div>
          <div className="settings-actions"><button type="button" onClick={exportAccounts}><Icon name="download" size={16} />导出脱敏配置</button><button type="button" onClick={() => importInputRef.current?.click()} disabled={isImporting}><Icon name="folder" size={16} />{isImporting ? '导入中…' : '导入脱敏配置'}</button></div>
          <div className="settings-security-note"><Icon name="shieldCheck" size={15} /><span>导出文件不包含授权码或令牌；导入后需重新授权。</span></div>
          <input ref={importInputRef} type="file" accept="application/json,.json" hidden onChange={importAccounts} />
        </motion.div>}
      </AnimatePresence>

      <AnimatePresence>{isHelpOpen && <HelpModal onClose={() => setHelpOpen(false)} />}</AnimatePresence>
      <AnimatePresence>{isMailRulesOpen && <MailRuleManager accounts={accounts} rules={mailRules} initialAccountId={selectedAccountId} busyKey={mailRuleBusyKey} externalError={mailRuleError} onAdd={addMailRule} onRemove={removeMailRule} onClose={() => { if (!mailRuleBusyKey) setMailRulesOpen(false) }} />}</AnimatePresence>
      <AnimatePresence>{pendingExternalLink && <ExternalLinkDialog inspection={pendingExternalLink} onClose={() => setPendingExternalLink(null)} onOpen={openExternalUrl} onError={(message) => pushToast(message, 'error')} />}</AnimatePresence>
      <AnimatePresence>{pendingOutboxDiscard && <ConfirmDialog
        title="删除这封待发送邮件？"
        detail={`“${pendingOutboxDiscard.subject || '(无主题)'}”及其本地草稿会一起删除，此操作无法撤销。`}
        confirmLabel="删除待发送"
        busy={outboxAction?.id === pendingOutboxDiscard.id && outboxAction.kind === 'discard'}
        onCancel={() => setPendingOutboxDiscard(null)}
        onConfirm={() => { void discardQueuedMessage() }}
      />}</AnimatePresence>
      <AnimatePresence>{isComposeOpen && <ComposeModal
        mode={composeMode}
        source={composeSource}
        accountId={composeSource?.accountId ?? selectedAccountId ?? accounts[0]?.id}
        senderEmail={composeAccount?.email}
        signature={composeAccount?.signature ?? ''}
        draftId={composeDraftId}
        onDraftChanged={handleDraftChanged}
        onDraftRemoved={handleDraftRemoved}
        onClose={() => { setComposeOpen(false); setComposeDraftId(undefined); setComposeMode('new'); setComposeSource(undefined); void refreshNativeDrafts() }}
        onSent={handleComposeSent}
        onError={(message) => pushToast(message, 'error')}
      />}</AnimatePresence>
      <AnimatePresence>{isAccountModalOpen && <AccountModal editingAccountId={editingAccountId} provider={provider} setProvider={changeProvider} providerDefinition={selectedProvider} accountEmail={accountEmail} setAccountEmail={setAccountEmail} authorizationCode={authorizationCode} setAuthorizationCode={setAuthorizationCode} showAuthorizationCode={showAuthorizationCode} setShowAuthorizationCode={setShowAuthorizationCode} customImapHost={customImapHost} setCustomImapHost={setCustomImapHost} customImapPort={customImapPort} setCustomImapPort={setCustomImapPort} customImapSecurity={customImapSecurity} setCustomImapSecurity={setCustomImapSecurity} customSmtpHost={customSmtpHost} setCustomSmtpHost={setCustomSmtpHost} customSmtpPort={customSmtpPort} setCustomSmtpPort={setCustomSmtpPort} customSmtpSecurity={customSmtpSecurity} setCustomSmtpSecurity={setCustomSmtpSecurity} customAuthentication={customAuthentication} setCustomAuthentication={setCustomAuthentication} deviceFlow={deviceFlow} diagnostic={editingAccountId ? connectionDiagnostics[editingAccountId] : undefined} isBusy={isAddingAccount} onClose={closeAccountModal} onOpenProvider={() => { void handleOpenProvider() }} onCopy={handleCopy} onAdd={handleAddAccount} onRemove={handleRemoveAccount} onDiagnose={() => { void handleDiagnoseAccount() }} />}</AnimatePresence>
      <div className="toast-stack" aria-live="polite">{toasts.map((toast) => <ToastView key={toast.id} toast={toast} onAction={(item) => { void handleUndoSend(item) }} />)}</div>
    </div>
  )
}

function HelpModal({ onClose }: { onClose: () => void }) {
  return <motion.div className="modal-backdrop help-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}><motion.div className="help-modal" initial={{ opacity: 0, y: 14, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 8, scale: 0.98 }} role="dialog" aria-modal="true" aria-labelledby="help-title"><div className="modal-header"><div><Icon name="help" size={21} /><h2 id="help-title">MailGo 帮助中心</h2></div><TooltipButton label="关闭帮助中心" onClick={onClose}><Icon name="close" size={19} /></TooltipButton></div><div className="help-modal-body"><section><h3>常用快捷键</h3><div className="shortcut-row"><span>写邮件</span><kbd>C</kbd></div><div className="shortcut-row"><span>聚焦搜索</span><kbd>Ctrl K</kbd></div><div className="shortcut-row"><span>回复当前邮件</span><kbd>R</kbd></div><div className="shortcut-row"><span>关闭弹窗</span><kbd>Esc</kbd></div></section><section><h3>账户与同步</h3><p>Google 与 Outlook 优先使用 OAuth 安全授权；QQ 邮箱和自定义 IMAP/SMTP 使用服务商生成的授权码或应用密码。</p><p>同步失败时，邮件仍保留在本地缓存；离线状态下的归档、删除和已读操作会在恢复连接后重放。</p></section><section><h3>隐私与安全</h3><p>授权凭据只交给本机安全存储。账户导出只包含脱敏元数据，导入另一台电脑后必须重新授权。HTML 邮件默认屏蔽远程图片，避免追踪像素；需要时可在设置中显式开启。</p></section></div><div className="modal-footer"><span><Icon name="shieldCheck" size={17} />MailGo 运行在本机，数据由你控制</span><button className="gradient-button" type="button" onClick={onClose}>知道了</button></div></motion.div></motion.div>
}

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
  providerDefinition: ReturnType<typeof providerFor>
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

function AccountModal({ editingAccountId, provider, setProvider, providerDefinition, accountEmail, setAccountEmail, authorizationCode, setAuthorizationCode, showAuthorizationCode, setShowAuthorizationCode, customImapHost, setCustomImapHost, customImapPort, setCustomImapPort, customImapSecurity, setCustomImapSecurity, customSmtpHost, setCustomSmtpHost, customSmtpPort, setCustomSmtpPort, customSmtpSecurity, setCustomSmtpSecurity, customAuthentication, setCustomAuthentication, deviceFlow, diagnostic, isBusy, onClose, onOpenProvider, onCopy, onAdd, onRemove, onDiagnose }: AccountModalProps) {
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

function ComposeModal({ mode, source, accountId, senderEmail, signature = '', draftId: openDraftId, onClose, onSent, onError, onDraftChanged, onDraftRemoved }: { mode: ComposeMode; source?: MailMessage; accountId?: string; senderEmail?: string; signature?: string; draftId?: string; onClose: () => void; onSent: (result: NativeSendResponse) => void; onError: (message: string) => void; onDraftChanged?: (draft: NativeDraft) => void; onDraftRemoved?: (draftId: string) => void }) {
  const [to, setTo] = useState('')
  const [cc, setCc] = useState('')
  const [bcc, setBcc] = useState('')
  const [showCopyFields, setShowCopyFields] = useState(false)
  const [subject, setSubject] = useState('')
  const [body, setBody] = useState('')
  const [richBody, setRichBody] = useState('')
  const [htmlMode, setHtmlMode] = useState(false)
  const [inReplyTo, setInReplyTo] = useState<string | undefined>()
  const [references, setReferences] = useState<string[]>([])
  const [draftId, setDraftId] = useState<string | undefined>()
  const [draftStatus, setDraftStatus] = useState('')
  const [draftReady, setDraftReady] = useState(false)
  const [attachments, setAttachments] = useState<ComposeAttachmentItem[]>([])
  const [inlineImages, setInlineImages] = useState<ComposeInlineImage[]>([])
  const [isSending, setSending] = useState(false)
  const [uploadingName, setUploadingName] = useState('')
  const fileInputRef = useRef<HTMLInputElement>(null)
  const imageInputRef = useRef<HTMLInputElement>(null)
  const attachmentsRef = useRef<ComposeAttachmentItem[]>([])
  const inlineImagesRef = useRef<ComposeInlineImage[]>([])
  const draftIdRef = useRef<string | undefined>(undefined)
  const pendingDraftSaveRef = useRef<Promise<NativeDraft | undefined>>(Promise.resolve(undefined))
  const pendingAttachmentJobsRef = useRef(new Set<Promise<void>>())
  const attachmentPersistenceQueueRef = useRef<Promise<void>>(Promise.resolve())
  const isNativeRuntime = Boolean(window.ipc?.postMessage)
  const maxAttachmentBytes = 25 * 1024 * 1024
  const maxTotalAttachmentBytes = 50 * 1024 * 1024
  const accountSignature = normalizeAccountSignature(signature)

  useEffect(() => {
    attachmentsRef.current = attachments
  }, [attachments])

  useEffect(() => {
    inlineImagesRef.current = inlineImages
  }, [inlineImages])

  useEffect(() => {
    draftIdRef.current = draftId
  }, [draftId])

  useEffect(() => () => {
    inlineImagesRef.current.forEach((image) => { if (image.previewUrl) URL.revokeObjectURL(image.previewUrl) })
  }, [])

  const clearInlineImages = () => {
    setInlineImages((current) => {
      current.forEach((image) => { if (image.previewUrl) URL.revokeObjectURL(image.previewUrl) })
      inlineImagesRef.current = []
      return []
    })
  }

  const saveDraftSnapshot = useCallback((force = false) => {
    if (!isNativeRuntime || !accountId || !draftReady || isSending) return Promise.resolve(undefined)
    if (!force && !draftIdRef.current && ![to, cc, bcc, subject, body].some((value) => value.trim())) return Promise.resolve(undefined)
    const save = async () => {
      setDraftStatus('正在保存草稿…')
      const draft = await invoke<NativeDraft>('drafts.save', {
        ...(draftIdRef.current ? { id: draftIdRef.current } : {}),
        accountId,
        to,
        cc,
        bcc,
        subject,
        body,
        htmlMode,
        ...(htmlMode && richBody.trim() ? { htmlBody: richBody } : {}),
        ...(inReplyTo ? { inReplyTo } : {}),
        ...(references.length ? { references } : {}),
      }, 30_000)
      draftIdRef.current = draft.id
      setDraftId(draft.id)
      setDraftStatus('草稿已自动保存')
      onDraftChanged?.(draft)
      return draft
    }
    const request = pendingDraftSaveRef.current.catch(() => undefined).then(save)
    pendingDraftSaveRef.current = request
    return request
  }, [accountId, bcc, body, cc, draftReady, htmlMode, inReplyTo, isNativeRuntime, isSending, onDraftChanged, references, richBody, subject, to])

  const waitForAttachmentJobs = useCallback(async () => {
    while (pendingAttachmentJobsRef.current.size > 0) {
      await Promise.allSettled(Array.from(pendingAttachmentJobsRef.current))
    }
  }, [])

  const trackAttachmentJob = useCallback((job: Promise<void>) => {
    pendingAttachmentJobsRef.current.add(job)
    void job.finally(() => pendingAttachmentJobsRef.current.delete(job)).catch(() => undefined)
  }, [])

  const loadDraftInlinePreview = useCallback(async (savedDraftId: string, attachment: NativeDraftAttachment) => {
    if (!accountId) return undefined
    let downloadId: string | undefined
    try {
      const start = await invoke<NativeAttachmentStartResponse>('drafts.attachment.start', {
        accountId,
        draftId: savedDraftId,
        attachmentId: attachment.id,
      }, 60_000)
      downloadId = start.downloadId
      let offset = 0
      let total = 0
      const parts: Uint8Array[] = []
      while (true) {
        const chunk: NativeAttachmentChunkResponse = await invoke<NativeAttachmentChunkResponse>('mail.attachment.chunk', { downloadId, offset }, 60_000)
        if (chunk.downloadId !== downloadId || chunk.offset !== offset || chunk.nextOffset < offset || chunk.nextOffset > start.size || (!chunk.done && chunk.nextOffset === offset)) {
          throw new Error('草稿图片传输响应无效')
        }
        const bytes = Uint8Array.from(atob(chunk.dataBase64), (character) => character.charCodeAt(0))
        if (chunk.nextOffset - offset !== bytes.length) throw new Error('草稿图片传输大小校验失败')
        parts.push(bytes)
        total += bytes.length
        if (total > start.size) throw new Error('草稿图片超过声明大小')
        offset = chunk.nextOffset
        if (chunk.done) break
      }
      if (total !== start.size) throw new Error('草稿图片传输不完整')
      const binary = new Uint8Array(total)
      let writeOffset = 0
      for (const part of parts) {
        binary.set(part, writeOffset)
        writeOffset += part.length
      }
      return URL.createObjectURL(new Blob([binary], { type: start.contentType }))
    } catch (error) {
      if (downloadId) void invoke('mail.attachment.cancel', { downloadId }).catch(() => undefined)
      throw error
    }
  }, [accountId])

  useEffect(() => {
    let cancelled = false
    const seed = composeSeed(mode, source, senderEmail)
    const threadHeaders = buildComposeThreadHeaders(mode, source)
    setDraftReady(false)
    setDraftStatus('')
    draftIdRef.current = openDraftId
    setDraftId(openDraftId)
    setTo(seed.to)
    setCc(seed.cc)
    setBcc('')
    setShowCopyFields(Boolean(seed.cc))
    setSubject(seed.subject)
    setBody(seed.body)
    setRichBody('')
    setHtmlMode(false)
    setInReplyTo(threadHeaders.inReplyTo)
    setReferences(threadHeaders.references)
    attachmentsRef.current = []
    setAttachments([])
    clearInlineImages()
    if (!isNativeRuntime || !accountId || (!openDraftId && mode !== 'new')) {
      setDraftReady(true)
      return () => { cancelled = true }
    }
    void invoke<NativeDraft[]>('drafts.list', { accountId }, 30_000).then((drafts) => {
      const draft = openDraftId ? drafts.find((item) => item.id === openDraftId) : drafts[0]
      if (cancelled || !draft) return
      draftIdRef.current = draft.id
      setDraftId(draft.id)
      setTo(draft.to)
      setCc(draft.cc)
      setBcc(draft.bcc)
      setShowCopyFields(Boolean(draft.cc || draft.bcc))
      setSubject(draft.subject)
      setBody(draft.body)
      setRichBody(draft.htmlBody || (draft.htmlMode ? plainTextToComposeHtml(draft.body) : ''))
      setHtmlMode(draft.htmlMode)
      setInReplyTo(draft.inReplyTo)
      setReferences(draft.references)
      const restoredAttachments: ComposeAttachmentItem[] = []
      const restoredImages: ComposeInlineImage[] = []
      for (const attachment of draft.attachments ?? []) {
        const item = {
          localId: `restored-${attachment.id}`,
          persistedId: attachment.id,
          fileName: attachment.fileName,
          contentType: attachment.contentType,
          size: attachment.size,
          status: 'saved' as const,
        }
        if (attachment.contentId) restoredImages.push({ ...item, contentId: attachment.contentId })
        else restoredAttachments.push(item)
      }
      attachmentsRef.current = restoredAttachments
      inlineImagesRef.current = restoredImages
      setAttachments(restoredAttachments)
      setInlineImages(restoredImages)
      if (restoredImages.length) setHtmlMode(true)
      setDraftStatus(openDraftId ? '已恢复草稿' : '已恢复最近草稿')
      onDraftChanged?.(draft)
      void (async () => {
        for (const image of restoredImages) {
          if (cancelled || !image.persistedId) return
          const metadata = draft.attachments.find((attachment) => attachment.id === image.persistedId)
          if (!metadata) continue
          try {
            const previewUrl = await loadDraftInlinePreview(draft.id, metadata)
            if (!previewUrl) continue
            if (cancelled) {
              URL.revokeObjectURL(previewUrl)
              return
            }
            setInlineImages((current) => {
              const next = current.map((item) => item.persistedId === image.persistedId ? { ...item, previewUrl } : item)
              inlineImagesRef.current = next
              return next
            })
          } catch {
            if (!cancelled) setDraftStatus('草稿已恢复，部分图片预览暂不可用')
          }
        }
      })()
    }).catch(() => undefined).finally(() => {
      if (!cancelled) setDraftReady(true)
    })
    return () => { cancelled = true }
  }, [accountId, isNativeRuntime, loadDraftInlinePreview, mode, onDraftChanged, openDraftId, senderEmail, source])

  useEffect(() => {
    if (!isNativeRuntime || !accountId || !draftReady || isSending) return
    if (!draftIdRef.current && ![to, cc, bcc, subject, body].some((value) => value.trim())) return
    const timer = window.setTimeout(() => {
      void saveDraftSnapshot().catch(() => setDraftStatus('草稿保存失败，将在下次输入时重试'))
    }, 700)
    return () => window.clearTimeout(timer)
  }, [accountId, bcc, body, cc, draftReady, htmlMode, inReplyTo, isNativeRuntime, isSending, references, richBody, saveDraftSnapshot, subject, to])

  const addFiles = (event: React.ChangeEvent<HTMLInputElement>) => {
    const incoming = Array.from(event.target.files ?? [])
    event.target.value = ''
    let total = attachments.reduce((sum, attachment) => sum + attachment.size, 0)
      + inlineImages.reduce((sum, image) => sum + image.size, 0)
    const accepted: ComposeAttachmentItem[] = []
    for (const file of incoming) {
      if (attachments.length + inlineImages.length + accepted.length >= 10) {
        onError('单封邮件最多添加 10 个附件')
        break
      }
      if (file.size > maxAttachmentBytes) {
        onError(file.name + ' 超过单个附件 25 MB 限制')
        continue
      }
      if (total + file.size > maxTotalAttachmentBytes) {
        onError('附件总大小不能超过 50 MB')
        break
      }
      accepted.push({
        localId: createComposeAttachmentId(),
        fileName: file.name,
        contentType: file.type || 'application/octet-stream',
        size: file.size,
        file,
        status: isNativeRuntime ? 'saving' : 'saved',
      })
      total += file.size
    }
    if (accepted.length) {
      setAttachments((current) => {
        const next = [...current, ...accepted]
        attachmentsRef.current = next
        return next
      })
      if (isNativeRuntime) {
        const job = attachmentPersistenceQueueRef.current.catch(() => undefined).then(() => persistAttachmentItems(accepted))
        attachmentPersistenceQueueRef.current = job
        trackAttachmentJob(job)
      }
    }
  }

  const addInlineImages = (event: React.ChangeEvent<HTMLInputElement>) => {
    const incoming = Array.from(event.target.files ?? [])
    event.target.value = ''
    let total = attachments.reduce((sum, attachment) => sum + attachment.size, 0)
      + inlineImages.reduce((sum, image) => sum + image.size, 0)
    const accepted: ComposeInlineImage[] = []
    for (const file of incoming) {
      if (!file.type.toLowerCase().startsWith('image/')) {
        onError(`${file.name} 不是受支持的图片格式`)
        continue
      }
      if (attachments.length + inlineImages.length + accepted.length >= 10) {
        onError('单封邮件最多添加 10 个附件或内嵌图片')
        break
      }
      if (file.size > maxAttachmentBytes) {
        onError(file.name + ' 超过单个图片 25 MB 限制')
        continue
      }
      if (total + file.size > maxTotalAttachmentBytes) {
        onError('附件和内嵌图片总大小不能超过 50 MB')
        break
      }
      accepted.push({
        localId: createComposeAttachmentId(),
        fileName: file.name,
        contentType: file.type || 'application/octet-stream',
        size: file.size,
        file,
        contentId: createInlineContentId(),
        previewUrl: URL.createObjectURL(file),
        status: isNativeRuntime ? 'saving' : 'saved',
      })
      total += file.size
    }
    if (accepted.length) {
      setInlineImages((current) => {
        const next = [...current, ...accepted]
        inlineImagesRef.current = next
        return next
      })
      setHtmlMode(true)
      if (isNativeRuntime) {
        const job = attachmentPersistenceQueueRef.current.catch(() => undefined).then(() => persistAttachmentItems(accepted))
        attachmentPersistenceQueueRef.current = job
        trackAttachmentJob(job)
      }
    }
  }

  const uploadAttachment = async (file: File, contentId?: string) => {
    const start = await invoke<NativeAttachmentUploadStartResponse>('mail.attachment.upload.start', {
      fileName: file.name,
      contentType: file.type || 'application/octet-stream',
      size: file.size,
      ...(contentId ? { contentId } : {}),
    }, 60_000)
    if (start.done) return start.uploadId
    const chunkSize = Math.min(Math.max(1, start.chunkSize), 192 * 1024)
    let offset = 0
    try {
      while (offset < file.size) {
        const nextOffset = Math.min(file.size, offset + chunkSize)
        const bytes = new Uint8Array(await file.slice(offset, nextOffset).arrayBuffer())
        const result = await invoke<NativeAttachmentUploadChunkResponse>('mail.attachment.upload.chunk', {
          uploadId: start.uploadId,
          offset,
          dataBase64: bytesToBase64(bytes),
        }, 60_000)
        if (result.uploadId !== start.uploadId || result.offset !== offset || result.nextOffset !== nextOffset || result.nextOffset <= offset || result.nextOffset > file.size) {
          throw new Error('附件上传响应无效')
        }
        offset = result.nextOffset
        setUploadingName(file.name + ' ' + Math.round((offset / file.size) * 100) + '%')
        if (result.done !== (offset === file.size)) throw new Error('附件上传完成状态无效')
      }
      return start.uploadId
    } catch (error) {
      void invoke('mail.attachment.upload.cancel', { uploadId: start.uploadId }).catch(() => undefined)
      throw error
    }
  }

  const persistAttachmentItems = async (items: Array<ComposeAttachmentItem | ComposeInlineImage>) => {
    let draft: NativeDraft | undefined
    try {
      draft = await saveDraftSnapshot(true)
      if (!draft) throw new Error('无法创建本地草稿')
    } catch {
      const failedIds = new Set(items.map((item) => item.localId))
      setAttachments((current) => {
        const next = current.map((entry) => failedIds.has(entry.localId) ? { ...entry, status: 'failed' as const } : entry)
        attachmentsRef.current = next
        return next
      })
      setInlineImages((current) => {
        const next = current.map((entry) => failedIds.has(entry.localId) ? { ...entry, status: 'failed' as const } : entry)
        inlineImagesRef.current = next
        return next
      })
      setDraftStatus('附件无法写入本地草稿；关闭前请移除，或直接发送')
      onError('无法创建本地草稿，附件尚未安全保存')
      return
    }
    let cursor = 0
    const worker = async () => {
      while (cursor < items.length) {
        const item = items[cursor++]
        if (!item.file) continue
        let uploadId: string | undefined
        const expectedContentId = 'contentId' in item ? item.contentId : undefined
        const metadataMatches = (attachment: NativeDraftAttachment) => (
          attachment.id === item.localId
          && attachment.fileName === item.fileName
          && attachment.contentType === item.contentType
          && attachment.contentId === expectedContentId
          && attachment.size === item.size
        )
        const markSaved = (savedDraft: NativeDraft, attachment: NativeDraftAttachment) => {
          const update = <T extends ComposeAttachmentItem>(current: T[]) => current.map((entry) => (
            entry.localId === item.localId
              ? { ...entry, file: undefined, persistedId: attachment.id, status: 'saved' as const }
              : entry
          ))
          if ('contentId' in item) {
            setInlineImages((current) => {
              const next = update(current)
              inlineImagesRef.current = next
              return next
            })
          } else {
            setAttachments((current) => {
              const next = update(current)
              attachmentsRef.current = next
              return next
            })
          }
          setDraftStatus('附件已加密保存到草稿')
          onDraftChanged?.(savedDraft)
        }
        try {
          setUploadingName('正在加密保存 ' + item.fileName)
          uploadId = await uploadAttachment(item.file, expectedContentId)
          const result = await invoke<{ draft: NativeDraft; attachment: NativeDraftAttachment }>('drafts.attachment.commit', {
            accountId,
            draftId: draft.id,
            attachmentId: item.localId,
            uploadId,
          }, 60_000)
          uploadId = undefined
          if (!metadataMatches(result.attachment)) {
            throw new Error('草稿附件保存响应无效')
          }
          markSaved(result.draft, result.attachment)
        } catch {
          if (uploadId) void invoke('mail.attachment.upload.cancel', { uploadId }).catch(() => undefined)
          try {
            const savedDrafts = await invoke<NativeDraft[]>('drafts.list', { accountId }, 30_000)
            const recoveredDraft = savedDrafts.find((candidate) => candidate.id === draft.id)
            const recoveredAttachment = recoveredDraft?.attachments.find((attachment) => attachment.id === item.localId)
            if (recoveredDraft && recoveredAttachment && metadataMatches(recoveredAttachment)) {
              markSaved(recoveredDraft, recoveredAttachment)
              continue
            }
          } catch {
            // The original failure is reflected as a recoverable local-only attachment below.
          }
          const markFailed = <T extends ComposeAttachmentItem>(current: T[]) => current.map((entry) => (
            entry.localId === item.localId ? { ...entry, status: 'failed' as const } : entry
          ))
          if ('contentId' in item) {
            setInlineImages((current) => {
              const next = markFailed(current)
              inlineImagesRef.current = next
              return next
            })
          } else {
            setAttachments((current) => {
              const next = markFailed(current)
              attachmentsRef.current = next
              return next
            })
          }
          setDraftStatus('部分附件保存失败；发送前会重试，关闭窗口前请先处理')
          onError(`${item.fileName} 未能保存到本地草稿`)
        }
      }
    }
    await Promise.all(Array.from({ length: Math.min(3, items.length) }, () => worker()))
    try {
      const latestDrafts = await invoke<NativeDraft[]>('drafts.list', { accountId }, 30_000)
      const latestDraft = latestDrafts.find((candidate) => candidate.id === draft.id)
      if (latestDraft) onDraftChanged?.(latestDraft)
    } catch {
      // Per-item commit responses already updated the UI; a later draft refresh will reconcile it.
    }
    const hasFailure = [...attachmentsRef.current, ...inlineImagesRef.current]
      .some((attachment) => attachment.status === 'failed')
    setDraftStatus(hasFailure ? '部分附件保存失败；发送前会重试，关闭窗口前请先处理' : '附件已加密保存到草稿')
    setUploadingName('')
  }

  const removeComposeAttachment = async (item: ComposeAttachmentItem | ComposeInlineImage, inline: boolean) => {
    if (item.status === 'saving' || item.status === 'removing') return
    const removeLocal = () => {
      if (inline && 'previewUrl' in item && item.previewUrl) URL.revokeObjectURL(item.previewUrl)
      if (inline) setInlineImages((current) => {
        const next = current.filter((entry) => entry.localId !== item.localId)
        inlineImagesRef.current = next
        return next
      })
      else setAttachments((current) => {
        const next = current.filter((entry) => entry.localId !== item.localId)
        attachmentsRef.current = next
        return next
      })
    }
    if (!isNativeRuntime || !accountId || !draftIdRef.current || !item.persistedId) {
      removeLocal()
      return
    }
    const mark = <T extends ComposeAttachmentItem>(current: T[], status: ComposeAttachmentStatus) => current.map((entry) => (
      entry.localId === item.localId ? { ...entry, status } : entry
    ))
    if (inline) setInlineImages((current) => {
      const next = mark(current, 'removing')
      inlineImagesRef.current = next
      return next
    })
    else setAttachments((current) => {
      const next = mark(current, 'removing')
      attachmentsRef.current = next
      return next
    })
    try {
      const draft = await invoke<NativeDraft>('drafts.attachment.remove', {
        accountId,
        draftId: draftIdRef.current,
        attachmentId: item.persistedId,
      }, 30_000)
      removeLocal()
      setDraftStatus('附件已从草稿移除')
      onDraftChanged?.(draft)
    } catch (error) {
      if (inline) setInlineImages((current) => {
        const next = mark(current, 'saved')
        inlineImagesRef.current = next
        return next
      })
      else setAttachments((current) => {
        const next = mark(current, 'saved')
        attachmentsRef.current = next
        return next
      })
      onError(error instanceof Error ? error.message : '附件移除失败')
    }
  }

  const requestClose = useCallback(async () => {
    if (isSending) return
    try {
      await waitForAttachmentJobs()
      const hasFailedAttachment = [...attachmentsRef.current, ...inlineImagesRef.current]
        .some((attachment) => attachment.status === 'failed')
      if (hasFailedAttachment) {
        setDraftStatus('有附件尚未安全保存；请移除后再关闭，或直接发送')
        onError('有附件未保存，为避免丢失，撰写窗口暂未关闭')
        return
      }
      const hasContent = [to, cc, bcc, subject, body].some((value) => value.trim())
      const currentDraftId = draftIdRef.current
      if (!hasContent && attachmentsRef.current.length === 0 && inlineImagesRef.current.length === 0 && isNativeRuntime && accountId && currentDraftId) {
        await invoke('drafts.remove', { accountId, id: currentDraftId }, 30_000)
        onDraftRemoved?.(currentDraftId)
        draftIdRef.current = undefined
        onClose()
        return
      }
      await saveDraftSnapshot(false)
      onClose()
    } catch (error) {
      setDraftStatus('关闭前保存失败，请重试')
      onError(error instanceof Error ? error.message : '关闭前保存草稿失败')
    }
  }, [accountId, bcc, body, cc, isNativeRuntime, isSending, onClose, onDraftRemoved, onError, saveDraftSnapshot, subject, to, waitForAttachmentJobs])

  useEffect(() => {
    const handleEscape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      if (event.target instanceof HTMLElement && event.target.closest('.rich-compose-link, .compose-schedule-menu')) return
      event.preventDefault()
      event.stopImmediatePropagation()
      void requestClose()
    }
    window.addEventListener('keydown', handleEscape, true)
    return () => window.removeEventListener('keydown', handleEscape, true)
  }, [requestClose])

  const discard = async () => {
    await waitForAttachmentJobs()
    const currentDraftId = draftIdRef.current
    if (!currentDraftId) {
      onClose()
      return
    }
    setSending(true)
    try {
      if (isNativeRuntime && accountId) {
        await invoke('drafts.remove', { accountId, id: currentDraftId }, 30_000)
        onDraftRemoved?.(currentDraftId)
      }
      onClose()
    } catch (error) {
      onError(error instanceof Error ? error.message : '草稿丢弃失败，请稍后重试')
    } finally {
      setSending(false)
    }
  }

  const send = async (scheduledFor?: number) => {
    if (!to.trim().includes('@')) {
      onError('请输入有效的收件人地址')
      return
    }
    if (isNativeRuntime && !accountId) {
      onError('请先添加一个可发送邮件的账户')
      return
    }
    setSending(true)
    const uploadIds: string[] = []
    let sendResult: NativeSendResponse | undefined
    try {
      if (isNativeRuntime) {
        await waitForAttachmentJobs()
        await saveDraftSnapshot(true)
        const currentAttachments = attachmentsRef.current
        const currentInlineImages = inlineImagesRef.current
        const allItems = [...currentAttachments, ...currentInlineImages]
        const missingAttachments = allItems.filter((item) => !item.persistedId && !item.file)
        if (missingAttachments.length) throw new Error('部分草稿附件无法读取，请移除后重试')
        const fallbackItems = allItems.filter((item): item is typeof item & { file: File } => !item.persistedId && Boolean(item.file))
        let cursor = 0
        const uploadedByIndex: string[] = []
        const worker = async () => {
          while (cursor < fallbackItems.length) {
            const index = cursor++
            const item = fallbackItems[index]
            setUploadingName('正在上传 ' + item.fileName)
            const contentId = typeof (item as Partial<ComposeInlineImage>).contentId === 'string'
              ? (item as ComposeInlineImage).contentId
              : undefined
            uploadedByIndex[index] = await uploadAttachment(item.file, contentId)
          }
        }
        await Promise.all(Array.from({ length: Math.min(3, fallbackItems.length) }, () => worker()))
        uploadIds.push(...uploadedByIndex)
        const persistedIds = allItems.flatMap((item) => item.persistedId ? [item.persistedId] : [])
        const currentDraftId = draftIdRef.current
        const effectiveHtml = htmlMode || currentInlineImages.length > 0
        const outgoingBody = appendAccountSignature(body, accountSignature)
        sendResult = await invoke<NativeSendResponse>('mail.send', {
          accountId,
          to: to.trim(),
          ...(cc.trim() ? { cc: cc.trim() } : {}),
          ...(bcc.trim() ? { bcc: bcc.trim() } : {}),
          subject: subject.trim() || '(无主题)',
          textBody: outgoingBody,
          ...(inReplyTo ? { inReplyTo } : {}),
          ...(references.length ? { references } : {}),
          ...(effectiveHtml && (outgoingBody.trim() || currentInlineImages.length) ? {
            htmlBody: composeHtmlBody(body, currentInlineImages, htmlMode ? richBody : undefined, accountSignature),
          } : {}),
          ...(uploadIds.length ? { attachmentIds: uploadIds } : {}),
          ...(currentDraftId ? { draftId: currentDraftId } : {}),
          ...(persistedIds.length ? { draftAttachmentIds: persistedIds } : {}),
          ...(scheduledFor ? { scheduledFor } : {}),
        })
      } else {
        await new Promise((resolve) => window.setTimeout(resolve, 700))
        sendResult = scheduledFor
          ? {
              sent: false,
              queued: true,
              accountId: accountId ?? 'demo-account',
              outboxId: `demo-${Date.now()}`,
              scheduled: true,
              scheduledFor,
              undoable: false,
            }
          : {
              sent: false,
              queued: true,
              accountId: accountId ?? 'demo-account',
              outboxId: `demo-${Date.now()}`,
              undoable: true,
              undoSeconds: DEFAULT_UNDO_SEND_SECONDS,
              undoExpiresAt: Date.now() + DEFAULT_UNDO_SEND_SECONDS * 1_000,
            }
      }
      const sentDraftId = draftIdRef.current
      if (isNativeRuntime && accountId && sentDraftId && !sendResult.queued) {
        await invoke('drafts.remove', { accountId, id: sentDraftId }, 30_000).catch(() => undefined)
        onDraftRemoved?.(sentDraftId)
      }
      onSent(sendResult)
    } catch (error) {
      await mapWithConcurrency(uploadIds, ACCOUNT_IPC_CONCURRENCY, (uploadId) => invoke('mail.attachment.upload.cancel', { uploadId }).catch(() => undefined))
      onError(error instanceof Error ? error.message : '邮件发送失败，请稍后重试')
    } finally {
      setUploadingName('')
      setSending(false)
    }
  }

  const composeTitle = mode === 'reply' ? '回复邮件' : mode === 'reply-all' ? '回复全部' : mode === 'forward' ? '转发邮件' : '新邮件'
  const htmlEnabled = htmlMode || inlineImages.length > 0
  return <motion.div className="modal-backdrop compose-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) void requestClose() }}>
    <motion.div className="compose-modal" initial={{ opacity: 0, y: 20 }} animate={{ opacity: 1 }} exit={{ opacity: 0, y: 20 }}>
      <div className="compose-header"><strong>{composeTitle}</strong><div><TooltipButton label="最小化撰写窗口"><span className="window-minimize" /></TooltipButton><TooltipButton label="关闭撰写窗口" onClick={() => { void requestClose() }}><Icon name="close" size={17} /></TooltipButton></div></div>
      <div className="compose-recipient-row"><RecipientInput fieldId="compose-to" label="收件人" autoFocus value={to} onChange={setTo} accountId={accountId} senderEmail={senderEmail} placeholder="姓名或邮箱，可用逗号分隔多个地址" /><button type="button" className="copy-fields-button" onClick={() => setShowCopyFields((value) => !value)} aria-expanded={showCopyFields}>{showCopyFields ? '隐藏抄送' : '抄送 / 密送'}</button></div>
      {showCopyFields && <><RecipientInput fieldId="compose-cc" label="抄送" value={cc} onChange={setCc} accountId={accountId} senderEmail={senderEmail} placeholder="输入姓名或邮箱" /><RecipientInput fieldId="compose-bcc" label="密送" value={bcc} onChange={setBcc} accountId={accountId} senderEmail={senderEmail} placeholder="输入姓名或邮箱" /></>}
      <label>主题<input value={subject} onChange={(event) => setSubject(event.target.value)} placeholder="主题" /></label>
      {htmlEnabled
        ? <RichTextEditor value={richBody || plainTextToComposeHtml(body)} placeholder="输入内容，将以 HTML + 纯文本双格式发送…" onChange={(html, plainText) => { setRichBody(html); setBody(plainText) }} onError={onError} />
        : <textarea className="compose-body" value={body} onChange={(event) => setBody(event.target.value)} placeholder="写下你的邮件…" />}
      {accountSignature && <div className="compose-signature-preview"><span>账户签名 · 发送时自动加入</span><p>{accountSignature}</p></div>}
      {draftStatus && <div className="compose-draft-status" aria-live="polite"><Icon name="cloud" size={14} />{draftStatus}</div>}
      {htmlEnabled && <div className="compose-format-note"><Icon name="grid" size={14} />将发送安全的 HTML + 纯文本版本{inlineImages.length > 0 ? '，内嵌图片会显示在正文末尾' : ''}</div>}
      {inlineImages.length > 0 && <div className="compose-inline-images" aria-label="待发送内嵌图片">{inlineImages.map((image) => <div className={`compose-inline-image is-${image.status}`} key={image.localId}>{image.previewUrl ? <img src={image.previewUrl} alt={image.fileName} /> : <span className="compose-inline-placeholder"><Icon name="image" size={22} /></span>}<span>{image.fileName}<small>{image.status === 'saving' ? '加密保存中…' : image.status === 'failed' ? '保存失败' : image.status === 'removing' ? '移除中…' : '已保存'}</small></span><button type="button" disabled={image.status === 'saving' || image.status === 'removing'} onClick={() => { trackAttachmentJob(removeComposeAttachment(image, true)) }} aria-label={'移除内嵌图片 ' + image.fileName}>×</button></div>)}</div>}
      {attachments.length > 0 && <div className="compose-attachments" aria-label="待发送附件">{attachments.map((attachment) => <div className={`compose-attachment is-${attachment.status}`} key={attachment.localId}><span>{attachment.fileName}</span><small>{Math.max(1, Math.round(attachment.size / 1024))} KB · {attachment.status === 'saving' ? '加密保存中…' : attachment.status === 'failed' ? '保存失败' : attachment.status === 'removing' ? '移除中…' : '已保存'}</small><button type="button" disabled={attachment.status === 'saving' || attachment.status === 'removing'} onClick={() => { trackAttachmentJob(removeComposeAttachment(attachment, false)) }} aria-label={'移除附件 ' + attachment.fileName}>×</button></div>)}</div>}
      {uploadingName && <div className="compose-uploading" aria-live="polite"><Icon name="rotate" size={14} />{uploadingName}</div>}
      <div className="compose-footer"><div><TooltipButton label="添加附件" onClick={() => fileInputRef.current?.click()}><Icon name="paperclip" size={19} /></TooltipButton><input ref={fileInputRef} type="file" multiple hidden onChange={addFiles} /><TooltipButton label="插入图片" onClick={() => imageInputRef.current?.click()}><Icon name="image" size={19} /></TooltipButton><input ref={imageInputRef} type="file" accept="image/*" multiple hidden onChange={addInlineImages} /><TooltipButton label={htmlEnabled ? (inlineImages.length > 0 ? '内嵌图片需要 HTML 格式' : '切换为纯文本（移除格式）') : '启用富文本格式'} active={htmlEnabled} onClick={() => { if (inlineImages.length > 0) { onError('已插入内嵌图片，HTML 格式必须保持开启'); return } if (htmlMode) { setRichBody(''); setHtmlMode(false) } else { setRichBody(plainTextToComposeHtml(body)); setHtmlMode(true) } }}><span className="reply-a">A</span></TooltipButton></div><div className="compose-send-actions">{draftId && <button type="button" className="danger-button" onClick={() => { void discard() }} disabled={isSending}><Icon name="trash" size={16} />丢弃草稿</button>}<ScheduleSendControl disabled={isSending || (isNativeRuntime && !draftReady)} label={isSending ? (uploadingName || '发送中…') : isNativeRuntime && !draftReady ? '准备中…' : '发送'} onSendNow={() => { void send() }} onSchedule={(timestamp) => { void send(timestamp) }} /></div></div>
    </motion.div>
  </motion.div>
}

export default App
