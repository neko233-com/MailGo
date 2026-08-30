import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Icon, type IconName } from './components/Icon'
import { folderLabels, providerDefinitions, sampleAccounts, sampleMails } from './data'
import { invoke, readNativeState } from './lib/ipc'
import type { FolderId, MailAccount, MailAttachment, MailMessage, NativeAttachmentChunkResponse, NativeAttachmentStartResponse, NativeAttachmentUploadChunkResponse, NativeAttachmentUploadStartResponse, NativeAuthStartResponse, NativeCachedMessage, NativeDeviceStartResponse, NativeDraft, NativeMailboxResponse, NativeMessageResponse, NativeQueueStatus, NativeSyncItem, NativeSyncResponse, Provider, SmartCategory, ThemeMode } from './types'

type ToastTone = 'info' | 'success' | 'error'
type Toast = { id: number; message: string; tone: ToastTone }
type DeviceFlowState = { sessionId: string; userCode: string; verificationUri: string; message?: string; retryAfter: number; status: 'pending' | 'complete' | 'error' }
type ActionMenu = 'bulk' | 'message'
type TransferMode = 'export-encrypted' | 'import-encrypted'

const smartCategories: { id: SmartCategory; label: string; icon: IconName; color: string }[] = [
  { id: 'apple-connect', label: 'Apple Connect', icon: 'shieldCheck', color: '#9ca6ba' },
  { id: 'apple-ads', label: 'Apple 广告', icon: 'grid', color: '#ed7191' },
  { id: 'social', label: '社交通知', icon: 'message', color: '#46cfa1' },
  { id: 'ads', label: '其他广告', icon: 'bell', color: '#f0a868' },
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

function formatCount(value: number) {
  return value > 99 ? '99+' : String(value)
}

function sanitizeHtml(input: string, allowRemoteImages = false) {
  const documentParser = new DOMParser().parseFromString(input, 'text/html')
  documentParser.querySelectorAll('script, iframe, object, embed, form, link, meta, style').forEach((node) => node.remove())
  documentParser.querySelectorAll('*').forEach((node) => {
    Array.from(node.attributes).forEach((attribute) => {
      const name = attribute.name.toLowerCase()
      const value = attribute.value.trim().replace(/[\u0000-\u0020]+/g, '')
      const isSafeUrl = name === 'href'
        ? /^(https:\/\/|mailto:|#)/i.test(value)
        : name === 'src'
          ? /^(cid:|data:image\/(?:png|gif|jpe?g|webp);base64,)/i.test(value) || (allowRemoteImages && /^https:\/\//i.test(value))
          : false
      if (name.startsWith('on') || ['style', 'srcdoc', 'srcset', 'ping', 'formaction', 'xlink:href'].includes(name)) node.removeAttribute(attribute.name)
      if (['href', 'src', 'action'].includes(name) && !isSafeUrl) {
        node.removeAttribute(attribute.name)
      }
    })
  })
  documentParser.querySelectorAll('a').forEach((node) => {
    if (node.getAttribute('target') === '_blank') {
      node.setAttribute('rel', 'noreferrer noopener')
    } else {
      node.removeAttribute('target')
    }
  })
  return documentParser.body.innerHTML
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
  if (folder === 'inbox') return 'INBOX'
  if (folder === 'sent') return account.provider === 'google' ? '[Gmail]/Sent Mail' : account.provider === 'outlook' ? 'Sent Items' : 'Sent Messages'
  if (folder === 'drafts') return account.provider === 'google' ? '[Gmail]/Drafts' : 'Drafts'
  if (folder === 'spam') return account.provider === 'google' ? '[Gmail]/Spam' : account.provider === 'outlook' ? 'Junk Email' : 'Spam'
  if (folder === 'trash') return account.provider === 'google' ? '[Gmail]/Trash' : account.provider === 'outlook' ? 'Deleted Items' : 'Trash'
  return account.provider === 'google' ? '[Gmail]/All Mail' : 'Archive'
}

function nativeMailboxKey(accountId: string, folder: string) {
  return `${accountId}::${folder}`
}

function nativeAttachmentKind(contentType: string): MailAttachment['kind'] {
  if (contentType === 'application/pdf') return 'pdf'
  if (contentType.includes('spreadsheet') || contentType.includes('excel')) return 'sheet'
  if (contentType.startsWith('image/')) return 'image'
  return 'file'
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
  return {
    id: `${message.accountId}:${message.folder}:${message.uid}`,
    accountId: message.accountId,
    folder: uiFolderForNative(message.folder),
    category: nativeCategory(message.category),
    from: message.senderEmail || 'unknown@example.com',
    senderName,
    subject: message.subject || '(无主题)',
    preview: message.preview || message.textBody.slice(0, 240),
    timestamp,
    dateGroup,
    unread: message.unread,
    starred: message.starred,
    isAd: message.isAd,
    accent: account.accent,
    avatar: senderName.split(/\s+/).map((part) => part[0]).join('').slice(0, 2).toUpperCase() || '?',
    body: message.textBody ? message.textBody.split(/\r?\n\s*\r?\n/).filter(Boolean) : ['正在加载邮件正文…'],
    attachments: message.attachments.map((attachment, index) => ({
      id: `${message.accountId}:${message.uid}:attachment:${index}`,
      name: attachment.fileName || 'attachment',
      size: attachment.size > 1024 * 1024 ? `${(attachment.size / 1024 / 1024).toFixed(1)} MB` : `${Math.max(1, Math.round(attachment.size / 1024))} KB`,
      kind: nativeAttachmentKind(attachment.contentType),
      nativeIndex: attachment.index,
    })),
    hasHtml: Boolean(message.htmlBody),
    htmlBody: message.htmlBody,
    nativeUid: message.uid,
    nativeFolder: message.folder,
  }
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

function loadTheme(): ThemeMode {
  const stored = window.localStorage.getItem('mailgo-theme')
  return stored === 'light' ? 'light' : 'dark'
}

function loadRemoteImages() {
  return window.localStorage.getItem('mailgo-remote-images') === 'true'
}

function loadHideAds() {
  return window.localStorage.getItem('mailgo-hide-ads') === 'true'
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
      <span className="brand-wave brand-wave-a" />
      <span className="brand-wave brand-wave-b" />
      <span className="brand-wave brand-wave-c" />
    </span>
  )
}

function TooltipButton({ label, onClick, children, active = false, className = '', ariaExpanded }: { label: string; onClick?: () => void; children: React.ReactNode; active?: boolean; className?: string; ariaExpanded?: boolean }) {
  return (
    <button className={`icon-button ${active ? 'is-active' : ''} ${className}`} onClick={onClick} aria-label={label} aria-expanded={ariaExpanded} title={label} type="button">
      {children}
    </button>
  )
}

function Avatar({ message, size = 'md' }: { message: MailMessage; size?: 'sm' | 'md' | 'lg' }) {
  return <span className={`avatar avatar-${size}`} style={{ '--avatar-accent': message.accent } as React.CSSProperties}>{message.avatar}</span>
}

function App() {
  const prefersReducedMotion = useReducedMotion()
  const [theme, setTheme] = useState<ThemeMode>(loadTheme)
  const [accounts, setAccounts] = useState<MailAccount[]>(sampleAccounts)
  const [mails, setMails] = useState<MailMessage[]>(() => sampleMails.map((mail) => ({ ...mail, body: [...mail.body], attachments: mail.attachments?.map((attachment) => ({ ...attachment })) })))
  const [selectedFolder, setSelectedFolder] = useState<FolderId>('inbox')
  const [selectedCategory, setSelectedCategory] = useState<SmartCategory | null>(null)
  const [selectedAccountId, setSelectedAccountId] = useState<string | null>(null)
  const [selectedMailId, setSelectedMailId] = useState('launch-plan')
  const [selectedMailIds, setSelectedMailIds] = useState<string[]>([])
  const [query, setQuery] = useState('')
  const [filterUnread, setFilterUnread] = useState(false)
  const [isComposeOpen, setComposeOpen] = useState(false)
  const [composeDraftId, setComposeDraftId] = useState<string | undefined>()
  const [isAccountModalOpen, setAccountModalOpen] = useState(false)
  const [isAuthPanelOpen, setAuthPanelOpen] = useState(true)
  const [isSettingsOpen, setSettingsOpen] = useState(false)
  const [openMenu, setOpenMenu] = useState<ActionMenu | null>(null)
  const [isHelpOpen, setHelpOpen] = useState(false)
  const [isSyncing, setSyncing] = useState(false)
  const [isLoadingEarlier, setLoadingEarlier] = useState(false)
  const [mailboxMeta, setMailboxMeta] = useState<Record<string, { oldestUid?: number; hasMore?: boolean }>>({})
  const [isHtmlMode, setHtmlMode] = useState(false)
  const [isImporting, setImporting] = useState(false)
  const [transferMode, setTransferMode] = useState<TransferMode | null>(null)
  const [transferFile, setTransferFile] = useState<File | null>(null)
  const [toasts, setToasts] = useState<Toast[]>([])
  const [minimizeToTray, setMinimizeToTray] = useState(true)
  const [notificationsEnabled, setNotificationsEnabled] = useState(true)
  const [remoteImagesEnabled, setRemoteImagesEnabled] = useState(loadRemoteImages)
  const [hideAds, setHideAds] = useState(loadHideAds)
  const [pendingOperations, setPendingOperations] = useState(0)
  const [nativeDrafts, setNativeDrafts] = useState<NativeDraft[]>([])
  const [provider, setProvider] = useState<Provider>('qq')
  const [accountEmail, setAccountEmail] = useState('')
  const [editingAccountId, setEditingAccountId] = useState<string | null>(null)
  const [authorizationCode, setAuthorizationCode] = useState('')
  const [oauthSessionId, setOauthSessionId] = useState('')
  const [oauthState, setOauthState] = useState('')
  const [deviceFlow, setDeviceFlow] = useState<DeviceFlowState | null>(null)
  const [showAuthorizationCode, setShowAuthorizationCode] = useState(false)
  const [customCss, setCustomCss] = useState(() => window.localStorage.getItem('mailgo-custom-css') ?? '')
  const [customImapHost, setCustomImapHost] = useState('imap.example.com')
  const [customImapPort, setCustomImapPort] = useState('993')
  const [customImapSecurity, setCustomImapSecurity] = useState('tls')
  const [customSmtpHost, setCustomSmtpHost] = useState('smtp.example.com')
  const [customSmtpPort, setCustomSmtpPort] = useState('465')
  const [customSmtpSecurity, setCustomSmtpSecurity] = useState('tls')
  const [customAuthentication, setCustomAuthentication] = useState('password')
  const [attachmentProgress, setAttachmentProgress] = useState<Record<string, number>>({})
  const importInputRef = useRef<HTMLInputElement>(null)
  const encryptedTransferInputRef = useRef<HTMLInputElement>(null)
  const accountPrefillRef = useRef<string | null>(null)
  const attachmentCancelsRef = useRef(new Map<string, () => void>())
  const isNativeRuntime = Boolean(window.ipc?.postMessage)

  useEffect(() => () => {
    attachmentCancelsRef.current.forEach((cancel) => cancel())
    attachmentCancelsRef.current.clear()
  }, [])

  const pushToast = (message: string, tone: ToastTone = 'info') => {
    const id = Date.now() + Math.random()
    setToasts((current) => [...current.slice(-2), { id, message, tone }])
    window.setTimeout(() => setToasts((current) => current.filter((toast) => toast.id !== id)), 3600)
  }

  const refreshPendingOperations = async (accountList: MailAccount[] = accounts) => {
    if (!isNativeRuntime) {
      setPendingOperations(0)
      return
    }
    try {
      const statuses = await Promise.all(accountList.map((account) => invoke<NativeQueueStatus>('sync.queue_status', { accountId: account.id })))
      setPendingOperations(statuses.reduce((total, status) => total + status.total, 0))
    } catch {
      // Queue status is telemetry for the local UI; a transient read failure must not interrupt mail actions.
    }
  }

  const refreshNativeDrafts = async (accountList: MailAccount[] = accounts) => {
    if (!isNativeRuntime) {
      setNativeDrafts([])
      return
    }
    try {
      const drafts = await Promise.all(accountList.map((account) => invoke<NativeDraft[]>('drafts.list', { accountId: account.id }, 30_000)))
      setNativeDrafts(drafts.flat().sort((left, right) => right.updatedAt - left.updatedAt))
    } catch {
      // A missing or unreadable draft cache must not make the inbox unavailable.
    }
  }

  const openCompose = (draftId?: string) => {
    setComposeDraftId(draftId)
    setComposeOpen(true)
  }

  const handleDraftChanged = useCallback((draft: NativeDraft) => {
    setNativeDrafts((current) => [...current.filter((item) => item.id !== draft.id), draft].sort((left, right) => right.updatedAt - left.updatedAt))
  }, [])

  const handleDraftRemoved = useCallback((draftId: string) => {
    setNativeDrafts((current) => current.filter((draft) => draft.id !== draftId))
  }, [])

  const changeProvider = (nextProvider: Provider) => {
    setProvider(nextProvider)
    setCustomAuthentication(nextProvider === 'outlook' || nextProvider === 'google' ? 'oauth2' : nextProvider === 'other' ? 'password' : 'app-password')
    setAuthorizationCode('')
    setOauthSessionId('')
    setOauthState('')
    setDeviceFlow(null)
  }

  const openNewAccount = () => {
    setEditingAccountId(null)
    changeProvider('qq')
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
    window.localStorage.setItem('mailgo-theme', theme)
  }, [theme])

  useEffect(() => {
    let cancelled = false
    void readNativeState().then(async (nativeState) => {
      if (cancelled || !nativeState) return
      if (isNativeRuntime) setAccounts(nativeState.accounts)
      setTheme(nativeState.theme)
      setMinimizeToTray(nativeState.minimizeToTray)
      setNotificationsEnabled(nativeState.notificationsEnabled ?? true)
      setRemoteImagesEnabled(nativeState.remoteImagesEnabled ?? false)
      setHideAds(nativeState.hideAds ?? false)
      void refreshPendingOperations(nativeState.accounts)
      void refreshNativeDrafts(nativeState.accounts)
      if (!isNativeRuntime) return
      await Promise.all(nativeState.accounts.map(async (account) => {
        try {
          const result = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id })
          const converted = (result.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, account))
          if (cancelled) return
          if (result.mailbox) {
            setMailboxMeta((current) => ({ ...current, [nativeMailboxKey(account.id, result.mailbox!.folder)]: { oldestUid: result.mailbox!.oldestUid, hasMore: result.mailbox!.hasMore } }))
          }
          setMails((current) => [...current.filter((mail) => mail.accountId !== account.id), ...converted])
          if (converted.length) setSelectedMailId((current) => current === 'launch-plan' ? converted[0].id : current)
        } catch {
          // An empty cache is a valid first-run state; sync will populate it later.
        }
      }))
    })
    return () => { cancelled = true }
  }, [isNativeRuntime])

  useEffect(() => {
    if (!isNativeRuntime) return
    let cancelled = false
    const refreshBackgroundStatuses = async () => {
      const nativeState = await readNativeState()
      if (cancelled || !nativeState) return
      setAccounts((current) => current.map((account) => {
        const refreshed = nativeState.accounts.find((item) => item.id === account.id)
        return refreshed
          ? { ...account, unread: refreshed.unread, status: refreshed.status, lastSync: refreshed.lastSync }
          : account
      }))
    }
    const timer = window.setInterval(() => { void refreshBackgroundStatuses() }, 30_000)
    return () => {
      cancelled = true
      window.clearInterval(timer)
    }
  }, [isNativeRuntime])

  useEffect(() => {
    const styleId = 'mailgo-user-theme'
    let style = document.getElementById(styleId) as HTMLStyleElement | null
    if (!style) {
      style = document.createElement('style')
      style.id = styleId
      document.head.appendChild(style)
    }
    style.textContent = customCss
    window.localStorage.setItem('mailgo-custom-css', customCss)
  }, [customCss])

  useEffect(() => {
    window.localStorage.setItem('mailgo-remote-images', String(remoteImagesEnabled))
  }, [remoteImagesEnabled])

  useEffect(() => {
    window.localStorage.setItem('mailgo-hide-ads', String(hideAds))
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
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
        event.preventDefault()
        document.getElementById('mail-search')?.focus()
      }
      if (event.key === 'Escape') {
        setComposeOpen(false)
        setComposeDraftId(undefined)
        setAccountModalOpen(false)
        setSettingsOpen(false)
        setOpenMenu(null)
        setHelpOpen(false)
      }
      if (!event.ctrlKey && !event.metaKey && event.key.toLowerCase() === 'c' && !(event.target as HTMLElement).matches('input, textarea, select')) {
        event.preventDefault()
        openCompose()
      }
    }
    window.addEventListener('keydown', handleShortcut)
    return () => window.removeEventListener('keydown', handleShortcut)
  }, [])

  const selectedProvider = providerFor(provider)
  const localDraftMails = useMemo(() => nativeDrafts.flatMap((draft) => {
    const account = accounts.find((item) => item.id === draft.accountId)
    return account ? [draftToUi(draft, account)] : []
  }), [accounts, nativeDrafts])
  const allMails = useMemo(() => isNativeRuntime ? [...mails, ...localDraftMails] : mails, [isNativeRuntime, localDraftMails, mails])
  const displayedFolderLabels = useMemo(() => {
    if (!isNativeRuntime) return folderLabels
    return folderLabels.map((folder) => ({
      ...folder,
      unread: folder.id === 'drafts'
        ? nativeDrafts.length
        : folder.id === 'starred'
          ? allMails.filter((mail) => mail.starred && mail.unread).length
          : allMails.filter((mail) => mail.folder === folder.id && mail.unread).length,
    }))
  }, [allMails, isNativeRuntime, nativeDrafts.length])
  const visibleMails = useMemo(() => {
    const lowerQuery = query.trim().toLowerCase()
    return allMails.filter((mail) => {
      const folderMatch = selectedFolder === 'starred' ? mail.starred : mail.folder === selectedFolder
      const accountMatch = !selectedAccountId || mail.accountId === selectedAccountId
      const categoryMatch = !selectedCategory || mail.category === selectedCategory
      const adMatch = !hideAds || !mail.isAd || Boolean(selectedCategory)
      const unreadMatch = !filterUnread || mail.unread
      const queryMatch = !lowerQuery || `${mail.senderName} ${mail.subject} ${mail.preview}`.toLowerCase().includes(lowerQuery)
      return folderMatch && accountMatch && categoryMatch && adMatch && unreadMatch && queryMatch
    })
  }, [allMails, filterUnread, hideAds, query, selectedAccountId, selectedCategory, selectedFolder])

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
  const selectedMailAccount = accounts.find((account) => account.id === selectedMail.accountId)

  const groupedMails = useMemo(() => {
    return visibleMails.reduce<Record<string, MailMessage[]>>((groups, mail) => {
      groups[mail.dateGroup] ??= []
      groups[mail.dateGroup].push(mail)
      return groups
    }, {})
  }, [visibleMails])

  const selectedVisibleMails = useMemo(
    () => visibleMails.filter((mail) => selectedMailIds.includes(mail.id)),
    [selectedMailIds, visibleMails],
  )
  const allVisibleSelected = visibleMails.length > 0 && selectedVisibleMails.length === visibleMails.length

  const canLoadEarlier = isNativeRuntime && selectedFolder !== 'starred' && accounts
    .filter((account) => !selectedAccountId || account.id === selectedAccountId)
    .some((account) => mailboxMeta[nativeMailboxKey(account.id, nativeFolderName(account, selectedFolder))]?.hasMore)

  const selectMail = async (mail: MailMessage) => {
    setSelectedMailId(mail.id)
    const localDraft = nativeDrafts.find((draft) => mail.id === `local-draft:${draft.accountId}:${draft.id}`)
    if (localDraft) {
      setSelectedAccountId(localDraft.accountId)
      openCompose(localDraft.id)
      return
    }
    if (mail.unread) setMails((current) => current.map((item) => item.id === mail.id ? { ...item, unread: false } : item))
    if (isNativeRuntime && mail.nativeUid) {
      void invoke('mail.mark_read', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: false }).then(() => refreshPendingOperations()).catch(() => undefined)
      try {
        const result = await invoke<NativeMessageResponse>('mail.get', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid })
        const account = accounts.find((item) => item.id === mail.accountId)
        if (account && result.message) {
          const converted = nativeMessageToUi(result.message, account)
          setMails((current) => current.map((item) => item.id === mail.id ? converted : item))
        }
      } catch {
        pushToast('邮件正文加载失败，仍可查看本地摘要', 'info')
      }
    }
  }

  const toggleStar = (mail: MailMessage) => {
    const nextStarred = !mail.starred
    setMails((current) => current.map((item) => item.id === mail.id ? { ...item, starred: nextStarred } : item))
    setSelectedMailId(mail.id)
    if (isNativeRuntime && mail.nativeUid) {
      void invoke('mail.star', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: nextStarred }).then(() => refreshPendingOperations()).catch(() => pushToast('星标同步失败，可稍后重试', 'error'))
    }
    pushToast(nextStarred ? '已添加到星标' : '已移出星标', 'success')
  }

  const toggleMailSelection = (mailId: string) => {
    setSelectedMailIds((current) => current.includes(mailId)
      ? current.filter((id) => id !== mailId)
      : [...current, mailId])
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
    setMails((current) => current.map((item) => item.id === mail.id ? { ...item, unread } : item))
    setAccounts((current) => current.map((account) => account.id === mail.accountId
      ? { ...account, unread: Math.max(0, account.unread + (unread ? 1 : -1)) }
      : account))
    if (isNativeRuntime && mail.nativeUid) {
      await invoke('mail.mark_read', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: unread })
      await refreshPendingOperations()
    }
  }

  const setSelectedReadState = async (unread: boolean) => {
    if (!selectedVisibleMails.length) {
      pushToast('请先选择邮件', 'info')
      return
    }
    const selected = selectedVisibleMails.filter((mail) => mail.unread !== unread)
    setMails((current) => current.map((mail) => selected.some((item) => item.id === mail.id) ? { ...mail, unread } : mail))
    setAccounts((current) => current.map((account) => {
      const count = selected.filter((mail) => mail.accountId === account.id).length
      return count ? { ...account, unread: Math.max(0, account.unread + (unread ? count : -count)) } : account
    }))
    let failed = 0
    if (isNativeRuntime) {
      const results = await Promise.all(selected.map(async (mail) => {
        if (!mail.nativeUid) return true
        try {
          await invoke('mail.mark_read', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: unread })
          return true
        } catch {
          return false
        }
      }))
      failed = results.filter((result) => !result).length
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
    setMails((current) => current.map((mail) => selected.some((item) => item.id === mail.id) ? { ...mail, starred } : mail))
    let failed = 0
    if (isNativeRuntime) {
      const results = await Promise.all(selected.map(async (mail) => {
        if (!mail.nativeUid) return true
        try {
          await invoke('mail.star', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: starred })
          return true
        } catch {
          return false
        }
      }))
      failed = results.filter((result) => !result).length
      await refreshPendingOperations()
    }
    setSelectedMailIds([])
    setOpenMenu(null)
    if (failed) pushToast(`${selected.length - failed} 封已处理，${failed} 封星标同步失败`, 'error')
    else pushToast(selected.length ? `已${starred ? '添加' : '移除'} ${selected.length} 封邮件的星标` : `所选邮件已经${starred ? '全部加星' : '全部取消星标'}`, 'success')
  }

  const moveMail = async (mail: MailMessage, operation: 'archive' | 'delete') => {
    if (mail.id === 'empty-mail') return false
    const account = accounts.find((item) => item.id === mail.accountId)
    if (operation === 'archive' && mail.folder === 'archive') return false
    const isPermanentDelete = operation === 'delete' && mail.folder === 'trash'
    const targetFolder = isPermanentDelete || !account ? undefined : nativeFolderName(account, operation === 'archive' ? 'archive' : 'trash')
    let queued = false
    if (isNativeRuntime && account && mail.nativeUid != null) {
      const result = await invoke<{ queued?: boolean }>('mail.' + operation, {
        accountId: mail.accountId,
        folder: mail.nativeFolder ?? 'INBOX',
        uid: mail.nativeUid,
        ...(targetFolder ? { targetFolder } : {}),
      })
      queued = Boolean(result.queued)
      await refreshPendingOperations()
    }
    if (isPermanentDelete) {
      setMails((current) => current.filter((item) => item.id !== mail.id))
      if (selectedMailId === mail.id) {
        const next = visibleMails.find((item) => item.id !== mail.id)
        setSelectedMailId(next?.id ?? '')
      }
    } else {
      const nextFolder = operation === 'archive' ? 'archive' : 'trash'
      setMails((current) => current.map((item) => item.id === mail.id
        ? { ...item, folder: nextFolder, nativeFolder: targetFolder ?? item.nativeFolder }
        : item))
    }
    if (mail.unread && mail.folder === 'inbox') {
      setAccounts((current) => current.map((item) => item.id === mail.accountId
        ? { ...item, unread: Math.max(0, item.unread - 1) }
        : item))
    }
    return queued
  }

  const runMove = async (mail: MailMessage, operation: 'archive' | 'delete') => {
    try {
      const queued = await moveMail(mail, operation)
      pushToast(queued ? '操作已保存，联网后会自动同步' : operation === 'archive' ? '邮件已归档' : '邮件已移入回收站', 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '邮件操作失败，请稍后重试', 'error')
    }
  }

  const applyBulkMove = async (operation: 'archive' | 'delete') => {
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
    else pushToast(`${operation === 'archive' ? `已归档 ${count} 封邮件` : `已将 ${count} 封邮件移入回收站`}${queued ? `，${queued} 封将在联网后同步` : ''}`, 'success')
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
    setSelectedCategory(null)
    setSelectedAccountId(null)
    setSelectedMailIds([])
    const first = allMails.find((mail) => folder === 'starred' ? mail.starred : mail.folder === folder)
    if (first) setSelectedMailId(first.id)
    if (isNativeRuntime && folder !== 'starred') {
      void Promise.all(accounts.map(async (account) => {
        try {
          const serverFolder = nativeFolderName(account, folder)
          const result = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id, folder: serverFolder })
          const converted = (result.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, account))
          if (result.mailbox) {
            setMailboxMeta((current) => ({ ...current, [nativeMailboxKey(account.id, serverFolder)]: { oldestUid: result.mailbox!.oldestUid, hasMore: result.mailbox!.hasMore } }))
          }
          setMails((current) => [...current.filter((mail) => !(mail.accountId === account.id && mail.nativeFolder === serverFolder)), ...converted])
        } catch {
          // A provider may not expose every optional folder; its cached copy remains untouched.
        }
      }))
    }
  }

  const loadEarlier = async () => {
    if (!isNativeRuntime || selectedFolder === 'starred' || isLoadingEarlier) return
    const targetAccounts = accounts.filter((account) => !selectedAccountId || account.id === selectedAccountId)
    const pendingAccounts = targetAccounts.filter((account) => mailboxMeta[nativeMailboxKey(account.id, nativeFolderName(account, selectedFolder))]?.hasMore)
    if (!pendingAccounts.length) {
      pushToast('已经加载到更早的邮件', 'info')
      return
    }
    setLoadingEarlier(true)
    try {
      let loaded = 0
      await Promise.all(pendingAccounts.map(async (account) => {
        const serverFolder = nativeFolderName(account, selectedFolder)
        const meta = mailboxMeta[nativeMailboxKey(account.id, serverFolder)]
        try {
          const pageResult = await invoke<NativeSyncItem>('sync.page', {
            accountId: account.id,
            folder: serverFolder,
            ...(meta?.oldestUid != null ? { beforeUid: meta.oldestUid } : {}),
            limit: 50,
          }, 60_000)
          const mailbox = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id, folder: serverFolder })
          if (!mailbox.mailbox) return
          loaded += pageResult.fetched
          setMailboxMeta((current) => ({ ...current, [nativeMailboxKey(account.id, serverFolder)]: { oldestUid: mailbox.mailbox!.oldestUid, hasMore: mailbox.mailbox!.hasMore } }))
          const converted = mailbox.mailbox.messages.map((message) => nativeMessageToUi(message, account))
          const incomingIds = new Set(converted.map((mail) => mail.id))
          setMails((current) => [...current.filter((mail) => !incomingIds.has(mail.id)), ...converted])
        } catch {
          pushToast(`${account.label} 的更早邮件加载失败，可稍后重试`, 'error')
        }
      }))
      if (loaded) pushToast(`已更新 ${loaded} 封本地邮件`, 'success')
    } finally {
      setLoadingEarlier(false)
    }
  }

  const selectCategory = (category: SmartCategory) => {
    setSelectedCategory(category)
    setSelectedFolder('inbox')
    setSelectedAccountId(null)
    const first = mails.find((mail) => mail.category === category)
    if (first) setSelectedMailId(first.id)
  }

  const handleSync = async () => {
    if (typeof navigator !== 'undefined' && !navigator.onLine) {
      pushToast('当前处于离线模式，已保留本地缓存；联网后可重试同步', 'info')
      return
    }
    setSyncing(true)
    try {
      const result = await invoke<NativeSyncResponse>('sync.all', {}, 60_000)
      setAccounts((current) => current.map((account) => {
        const synced = result.synced?.find((item) => item.accountId === account.id)
        const failed = result.failed?.find((item) => item.accountId === account.id)
        if (synced) return { ...account, unread: synced.unread, status: 'synced' as const, lastSync: '刚刚同步' }
        if (failed) return { ...account, status: failed.message.includes('authorization') ? 'needs-auth' as const : 'offline' as const, lastSync: failed.message.includes('authorization') ? '等待重新授权' : '同步失败，可重试' }
        return account
      }))
      if (result.failed?.length) pushToast(`${result.synced?.length ?? 0} 个账户已同步，${result.failed.length} 个需要处理`, 'info')
      else pushToast('所有账户已完成同步', 'success')
      if (isNativeRuntime) {
        await Promise.all(accounts.map(async (account) => {
          try {
            const mailbox = await invoke<NativeMailboxResponse>('mail.list', { accountId: account.id })
            const converted = (mailbox.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, account))
            if (mailbox.mailbox) {
              setMailboxMeta((current) => ({ ...current, [nativeMailboxKey(account.id, mailbox.mailbox!.folder)]: { oldestUid: mailbox.mailbox!.oldestUid, hasMore: mailbox.mailbox!.hasMore } }))
            }
            setMails((current) => [...current.filter((mail) => mail.accountId !== account.id), ...converted])
          } catch {
            // Preserve the previous offline copy if one account has a transient cache error.
          }
        }))
        await refreshPendingOperations(accounts)
      }
    } catch {
      // Browser preview has no account transport. Keep the design-time demo responsive.
      await new Promise((resolve) => window.setTimeout(resolve, 700))
      setAccounts((current) => current.map((account) => ({ ...account, status: 'synced', lastSync: '刚刚同步' })))
      pushToast('所有账户已完成同步', 'success')
    } finally {
      setSyncing(false)
    }
  }

  const handleOpenProvider = async () => {
    if (isNativeRuntime && customAuthentication === 'oauth2' && accountEmail.trim()) {
      try {
        if (provider === 'outlook') {
          const flow = await invoke<NativeDeviceStartResponse>('auth.device.start', {
            provider,
            email: accountEmail.trim(),
          })
          setOauthSessionId(flow.sessionId)
          setDeviceFlow({
            sessionId: flow.sessionId,
            userCode: flow.userCode,
            verificationUri: flow.verificationUri,
            message: flow.message,
            retryAfter: flow.interval,
            status: 'pending',
          })
          window.open(flow.verificationUri, '_blank', 'noopener,noreferrer')
          pushToast(`Outlook 设备码 ${flow.userCode} 已生成，完成验证后会自动检测`, 'info')
          return
        }
        const flow = await invoke<NativeAuthStartResponse>('auth.start', {
          provider,
          email: accountEmail.trim(),
        })
        setOauthSessionId(flow.sessionId)
        setOauthState(flow.state)
        window.open(flow.authorizationUrl, '_blank', 'noopener,noreferrer')
        pushToast('OAuth 授权页面已打开，完成后将授权码粘贴回来', 'info')
        return
      } catch (error) {
        pushToast(error instanceof Error ? error.message : 'OAuth 客户端尚未配置，将打开帮助页面', 'error')
      }
    }
    window.open(selectedProvider.authUrl, '_blank', 'noopener,noreferrer')
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
    const existingAccount = editingAccountId ? accounts.find((account) => account.id === editingAccountId) : undefined
    const id = editingAccountId ?? `${provider}-${Date.now()}`
    const newAccount: MailAccount = {
      id,
      provider,
      label: selectedProvider.label,
      email: accountEmail.trim(),
      unread: 0,
      accent: selectedProvider.accent,
      status: 'syncing',
      lastSync: '正在同步…',
      authentication: customAuthentication,
      ...(provider === 'other' ? {
        imapHost: customImapHost.trim(),
        imapPort: Number(customImapPort),
        imapSecurity: customImapSecurity,
        smtpHost: customSmtpHost.trim(),
        smtpPort: Number(customSmtpPort),
        smtpSecurity: customSmtpSecurity,
      } : {}),
    }
    setAccounts((current) => existingAccount
      ? current.map((account) => account.id === id ? newAccount : account)
      : [...current, newAccount])
    let accountStored = false
    try {
      await invoke('accounts.add', {
        id,
        provider,
        label: selectedProvider.label,
        email: accountEmail.trim(),
        authorizationCode,
        authentication: customAuthentication,
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
      accountStored = true
      const result = await invoke<{ unread?: number }>('sync.account', { accountId: id }, 60_000)
      setAccounts((current) => current.map((account) => account.id === id ? { ...account, unread: result.unread ?? 0, status: 'synced', lastSync: '刚刚同步' } : account))
      if (isNativeRuntime) {
        const mailbox = await invoke<NativeMailboxResponse>('mail.list', { accountId: id })
        const converted = (mailbox.mailbox?.messages ?? []).map((message) => nativeMessageToUi(message, newAccount))
        if (mailbox.mailbox) {
          setMailboxMeta((current) => ({ ...current, [nativeMailboxKey(id, mailbox.mailbox!.folder)]: { oldestUid: mailbox.mailbox!.oldestUid, hasMore: mailbox.mailbox!.hasMore } }))
        }
        setMails((current) => [...current.filter((mail) => mail.accountId !== id), ...converted])
        if (converted.length) setSelectedMailId(converted[0].id)
      }
    } catch (error) {
      if (accountStored) {
        const message = error instanceof Error ? error.message : ''
        const needsAuth = /auth|credential|login|password|authorization/i.test(message)
        setAccounts((current) => current.map((account) => account.id === id
          ? { ...newAccount, status: needsAuth ? 'needs-auth' : 'offline', lastSync: needsAuth ? '等待重新授权' : '首次同步失败，可重试' }
          : account))
        setAuthorizationCode('')
        setOauthSessionId('')
        setOauthState('')
        setDeviceFlow(null)
        setAccountEmail('')
        setEditingAccountId(null)
        setAccountModalOpen(false)
        setSelectedAccountId(id)
        pushToast(needsAuth ? '账户已保存，但需要重新授权' : '账户已保存，首次同步失败；可稍后点击账户重试', needsAuth ? 'error' : 'info')
        return
      }
      setAccounts((current) => existingAccount
        ? current.map((account) => account.id === existingAccount.id ? existingAccount : account)
        : current.filter((account) => account.id !== id))
      pushToast(existingAccount ? '账户重新授权失败，已保留原账户配置' : '账户添加失败，请检查授权码、OAuth 配置或服务器设置', 'error')
      return
    }
    await refreshPendingOperations([...accounts.filter((account) => account.id !== id), newAccount])
    setAuthorizationCode('')
    setOauthSessionId('')
    setOauthState('')
    setDeviceFlow(null)
    setAccountEmail('')
    setEditingAccountId(null)
    setAccountModalOpen(false)
    setSelectedAccountId(id)
    pushToast(`${selectedProvider.label}账户${existingAccount ? '已重新授权' : '已加入'}，正在同步邮件`, 'success')
  }

  const handleRemoveAccount = async () => {
    if (!editingAccountId) return
    const account = accounts.find((item) => item.id === editingAccountId)
    if (!account || !window.confirm(`确定移除 ${account.label}（${account.email}）吗？本机凭据与缓存也会删除。`)) return
    try {
      if (isNativeRuntime) await invoke('accounts.remove', { id: account.id })
      setAccounts((current) => current.filter((item) => item.id !== account.id))
      setMails((current) => current.filter((mail) => mail.accountId !== account.id))
      setNativeDrafts((current) => current.filter((draft) => draft.accountId !== account.id))
      setMailboxMeta((current) => Object.fromEntries(Object.entries(current).filter(([key]) => !key.startsWith(`${account.id}::`))))
      setSelectedAccountId((current) => current === account.id ? null : current)
      setEditingAccountId(null)
      setAccountModalOpen(false)
      pushToast(`${account.label} 已移除，本机凭据与缓存已清理`, 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '账户移除失败，请稍后重试', 'error')
    }
  }

  const exportAccounts = () => {
    const payload = {
      schemaVersion: 1,
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
    setImporting(true)
    try {
      const parsed = JSON.parse(await file.text()) as { accounts?: unknown[]; schemaVersion?: number }
      if (parsed.schemaVersion !== 1 || !Array.isArray(parsed.accounts)) throw new Error('不支持的配置格式')
      const imported = parsed.accounts.flatMap((candidate) => {
        if (!candidate || typeof candidate !== 'object') return []
        const account = candidate as Partial<MailAccount>
        if (typeof account.id !== 'string' || typeof account.email !== 'string' || !isSupportedProvider(account.provider)) return []
        const id = account.id.trim()
        const email = account.email.trim()
        if (!id || id.length > 128 || !email.includes('@') || email.length > 320) return []
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
        }]
      }).slice(0, 64)
      setAccounts((current) => [...current.filter((account) => !imported.some((item) => item.id === account.id)), ...imported])
      try { await invoke('accounts.import', { accounts: imported }) } catch { /* Browser preview fallback. */ }
      pushToast(`已导入 ${imported.length} 个账户，请逐一补充授权码`, 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '配置导入失败', 'error')
    } finally {
      setImporting(false)
    }
  }

  const openEncryptedExport = () => {
    if (!isNativeRuntime) {
      pushToast('浏览器预览不读取凭据；请在 Windows 桌面端导出加密配置', 'info')
      return
    }
    setSettingsOpen(false)
    setTransferFile(null)
    setTransferMode('export-encrypted')
  }

  const openEncryptedImport = () => {
    if (!isNativeRuntime) {
      pushToast('浏览器预览不导入凭据；请在 Windows 桌面端导入加密配置', 'info')
      return
    }
    setSettingsOpen(false)
    encryptedTransferInputRef.current?.click()
  }

  const selectEncryptedTransferFile = (event: React.ChangeEvent<HTMLInputElement>) => {
    const file = event.target.files?.[0]
    event.target.value = ''
    if (!file) return
    setTransferFile(file)
    setTransferMode('import-encrypted')
  }

  const handleEncryptedTransfer = async (passphrase: string) => {
    if (!transferMode || !isNativeRuntime) return
    setImporting(true)
    try {
      if (transferMode === 'export-encrypted') {
        const result = await invoke<{ bundle: string; accountCount: number }>('accounts.export_encrypted', { passphrase }, 30_000)
        const blob = new Blob([result.bundle], { type: 'application/json' })
        const url = URL.createObjectURL(blob)
        const anchor = document.createElement('a')
        anchor.href = url
        anchor.download = `mailgo-accounts-encrypted-${new Date().toISOString().slice(0, 10)}.json`
        anchor.click()
        URL.revokeObjectURL(url)
        pushToast(`已导出 ${result.accountCount} 个加密账户配置`, 'success')
      } else {
        if (!transferFile || transferFile.size > 8 * 1024 * 1024) throw new Error('加密配置文件必须小于 8 MB')
        const bundle = await transferFile.text()
        const result = await invoke<{ imported: number }>('accounts.import_encrypted', { bundle, passphrase }, 30_000)
        const nativeState = await readNativeState()
        if (nativeState) {
          setAccounts(nativeState.accounts)
          setMails((current) => current.filter((mail) => nativeState.accounts.some((account) => account.id === mail.accountId)))
          void refreshNativeDrafts(nativeState.accounts)
        }
        pushToast(`已导入 ${result.imported} 个账户，首次同步前缓存已清理`, 'success')
      }
      setTransferMode(null)
      setTransferFile(null)
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '加密账户配置处理失败', 'error')
    } finally {
      setImporting(false)
    }
  }

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

  return (
    <div className="app-shell">
      <style>{`.reicon { width: 1em; height: 1em; }`}</style>
      <header className="titlebar" data-rdesktop-drag="true" onDoubleClick={() => window.__RDESKTOP_WINDOW__?.maximize()}>
        <div className="titlebar-brand"><BrandMark /><span>MailGo</span></div>
        <div className="titlebar-center">统一收件箱</div>
        <div className="window-controls" data-no-drag="true">
          <TooltipButton label="最小化" onClick={() => window.__RDESKTOP_WINDOW__?.minimize()}><span className="window-minimize" /></TooltipButton>
          <TooltipButton label="最大化" onClick={() => window.__RDESKTOP_WINDOW__?.maximize()}><Icon name="maximize" size={16} /></TooltipButton>
          <TooltipButton label={minimizeToTray ? '缩小到托盘' : '关闭 MailGo'} onClick={handleCloseWindow} className="close-button"><Icon name="close" size={17} /></TooltipButton>
        </div>
      </header>

      <div className="workspace">
        <aside className="sidebar">
          <div className="sidebar-top">
            <button className="compose-button" type="button" onClick={() => openCompose()}><Icon name="edit" size={19} /><span>写邮件</span><span className="compose-shortcut">C</span></button>
            <nav className="folder-nav" aria-label="邮件文件夹">
              {displayedFolderLabels.map((folder) => (
                <button key={folder.id} type="button" className={`nav-row ${selectedFolder === folder.id && !selectedCategory ? 'is-selected' : ''}`} onClick={() => selectFolder(folder.id)}>
                  <span className="nav-icon"><Icon name={folder.icon as IconName} size={19} weight={selectedFolder === folder.id ? 'Filled' : 'Outline'} /></span>
                  <span>{folder.label}</span>
                  {folder.unread > 0 && <span className={`nav-count ${selectedFolder === folder.id ? 'nav-count-selected' : ''}`}>{formatCount(folder.unread)}</span>}
                </button>
              ))}
            </nav>
          </div>

          <div className="sidebar-section smart-section">
            <div className="section-label-row"><span>智能分类</span><TooltipButton label="管理分类" onClick={() => setSettingsOpen(true)}><Icon name="settings" size={15} /></TooltipButton></div>
            <div className="smart-list">
              {smartCategories.map((category) => (
                <button key={category.id} type="button" className={`smart-row ${selectedCategory === category.id ? 'is-selected' : ''}`} onClick={() => selectCategory(category.id)}>
                  <span className="smart-dot" style={{ background: category.color }}><Icon name={category.icon} size={14} /></span><span>{category.label}</span>
                </button>
              ))}
            </div>
          </div>

          <div className="sidebar-section accounts-section">
            <div className="section-label-row"><span>账户</span><TooltipButton label="添加账户" onClick={openNewAccount}><Icon name="add" size={16} /></TooltipButton></div>
            <div className="account-list">
              {accounts.map((account) => (
                <button key={account.id} type="button" className={`account-row ${selectedAccountId === account.id ? 'is-selected' : ''}`} onClick={() => { setSelectedAccountId(account.id); setSelectedCategory(null); setSelectedFolder('inbox'); setSelectedMailIds([]) }}>
                  <ProviderMark provider={account.provider} size="sm" />
                  <span className="account-copy"><strong>{account.label}</strong><small>{account.email}</small></span>
                  {account.unread > 0 && <span className="account-count">{account.unread}</span>}
                  <span className={`sync-dot sync-${account.status}`} aria-label={account.status === 'synced' ? '已同步' : account.status === 'offline' ? '离线' : '需要授权'} />
                </button>
              ))}
            </div>
          </div>

          <div className="storage-bar">
            <div className="storage-meta"><span>本地缓存</span><span>4.2 GB / 15 GB</span></div>
            <div className="storage-track"><span /></div>
            <div className="storage-foot"><span aria-live="polite"><Icon name={pendingOperations ? 'rotate' : 'cloud'} size={13} /> {pendingOperations ? `${pendingOperations} 项操作待同步` : '离线可查看最近邮件'}</span><button type="button" onClick={handleSync}><Icon name="rotate" size={13} /> {isSyncing ? '同步中…' : '立即同步'}</button></div>
          </div>

          <div className="sidebar-quick-settings">
            <button type="button" className={hideAds ? 'is-on' : ''} aria-label={hideAds ? '广告已屏蔽' : '广告已分类'} aria-pressed={hideAds} onClick={() => { const next = !hideAds; setHideAds(next); void invoke('app.set_hide_ads', { enabled: next }).catch(() => undefined) }}>
              <Icon name="shield" size={15} />
              <span>广告 {hideAds ? '已屏蔽' : '已分类'}</span>
              <small>{hideAds ? '普通列表隐藏' : '普通列表显示'}</small>
            </button>
          </div>

          <div className="sidebar-footer">
            <TooltipButton label="设置" active={isSettingsOpen} onClick={() => setSettingsOpen((value) => !value)}><Icon name="settings" size={19} /></TooltipButton>
            <TooltipButton label="帮助中心" active={isHelpOpen} onClick={() => { setOpenMenu(null); setHelpOpen(true) }}><Icon name="help" size={19} /></TooltipButton>
            <TooltipButton label="收起侧栏" className="sidebar-collapse"><Icon name="menu" size={19} /></TooltipButton>
          </div>
        </aside>

        <main className="mail-list-panel">
          <div className="panel-toolbar">
            <div className="search-wrap"><Icon name="search" size={19} /><input id="mail-search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索邮件" aria-label="搜索邮件" /><kbd>Ctrl K</kbd></div>
            <button className={`filter-button ${filterUnread ? 'is-active' : ''}`} type="button" onClick={() => setFilterUnread((value) => !value)}><Icon name="filter" size={17} /> 筛选{filterUnread && <span className="filter-dot" />}</button>
          </div>
          <div className="list-toolbar">
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
                <button type="button" role="menuitem" disabled={!selectedVisibleMails.length} onClick={() => { setSelectedMailIds([]); setOpenMenu(null) }}><Icon name="close" size={16} />取消选择</button>
              </div>}
            </div>
          </div>
          <div className="mail-list-scroll">
            <AnimatePresence initial={false} mode="popLayout">
              {Object.entries(groupedMails).map(([group, mails]) => (
                <motion.section key={group} className="mail-group" initial={prefersReducedMotion ? false : { opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}>
                  <div className="mail-group-label">{group}</div>
                  {mails.map((mail) => (
                    <motion.div layout key={mail.id} className={`mail-row ${selectedMailId === mail.id ? 'is-selected' : ''} ${mail.unread ? 'is-unread' : ''}`} onClick={() => selectMail(mail)} whileHover={prefersReducedMotion ? undefined : { y: -1 }} transition={{ duration: 0.16 }}>
                      <label className="checkbox-wrap row-checkbox" onClick={(event) => event.stopPropagation()}><input type="checkbox" aria-label={`选择 ${mail.subject}`} checked={selectedMailIds.includes(mail.id)} onChange={() => toggleMailSelection(mail.id)} /><span /></label>
                      <Avatar message={mail} size="md" />
                      <div className="mail-row-copy"><div className="mail-row-top"><strong>{mail.senderName}</strong><time>{mail.timestamp}</time></div><div className="mail-row-subject">{mail.subject}</div><p>{mail.preview}</p></div>
                      <button type="button" className={`star-button ${mail.starred ? 'is-starred' : ''}`} aria-label={mail.starred ? '取消星标' : '添加星标'} onClick={(event) => { event.stopPropagation(); toggleStar(mail) }}><Icon name="star" size={19} weight={mail.starred ? 'Filled' : 'Outline'} /></button>
                    </motion.div>
                  ))}
                </motion.section>
              ))}
            </AnimatePresence>
            {visibleMails.length === 0 && <div className="empty-list"><span className="empty-icon"><Icon name="search" size={24} /></span><strong>没有找到邮件</strong><p>试试清除筛选或搜索其他关键词。</p></div>}
          </div>
          <div className="list-footer"><span>{visibleMails.length ? `1–${visibleMails.length} / ${visibleMails.length}` : '0 封邮件'}</span><div className="list-footer-actions">{canLoadEarlier && <button type="button" className="load-earlier-button" onClick={() => { void loadEarlier() }} disabled={isLoadingEarlier}>{isLoadingEarlier ? '加载中…' : '加载更早邮件'}</button>}<TooltipButton label="刷新邮件" onClick={handleSync}><Icon name="rotate" size={17} /></TooltipButton></div></div>
        </main>

        <section className="reading-panel" aria-label="邮件阅读区">
          <div className="reading-toolbar">
            <div className="reading-actions"><TooltipButton label="回复" onClick={() => openCompose()}><Icon name="reply" size={18} /></TooltipButton><span>回复</span><TooltipButton label="回复全部" onClick={() => openCompose()}><Icon name="reply" size={18} /></TooltipButton><span>回复全部</span><TooltipButton label="转发" onClick={() => openCompose()}><Icon name="forward" size={18} /></TooltipButton><span>转发</span><TooltipButton label="归档" onClick={() => { void runMove(selectedMail, 'archive') }}><Icon name="archive" size={18} /></TooltipButton><span>归档</span><TooltipButton label="删除" onClick={() => { void runMove(selectedMail, 'delete') }}><Icon name="trash" size={18} /></TooltipButton><span>删除</span></div>
            <div className="menu-anchor">
              <TooltipButton label="更多邮件操作" active={openMenu === 'message'} ariaExpanded={openMenu === 'message'} onClick={() => setOpenMenu((current) => current === 'message' ? null : 'message')}><Icon name="more" size={19} /></TooltipButton>
              {openMenu === 'message' && <div className="action-menu" role="menu" aria-label="更多邮件操作">
                <button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail'} onClick={() => { void markSelectedMessageUnread() }}><Icon name="message" size={16} />标为未读</button>
                <button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail'} onClick={() => { void copySelectedMessage() }}><Icon name="copy" size={16} />复制邮件正文</button>
                <button type="button" role="menuitem" disabled={selectedMail.id === 'empty-mail'} onClick={() => { setOpenMenu(null); window.print() }}><Icon name="document" size={16} />打印邮件</button>
              </div>}
            </div>
          </div>
          <div className="reading-scroll">
            <div className="reading-heading"><div><h1>{selectedMail.subject}</h1><div className="message-tags"><span className="tag tag-account"><ProviderMark provider={accounts.find((account) => account.id === selectedMail.accountId)?.provider ?? 'google'} size="sm" /> {accounts.find((account) => account.id === selectedMail.accountId)?.label ?? 'Google'}</span>{selectedMail.hasHtml && <span className="tag">HTML 邮件</span>}</div></div><TooltipButton label={selectedMail.starred ? '取消星标' : '添加星标'} className={`reading-star ${selectedMail.starred ? 'is-starred' : ''}`} onClick={() => toggleStar(selectedMail)}><Icon name="star" size={24} weight={selectedMail.starred ? 'Filled' : 'Outline'} /></TooltipButton></div>
            <div className="sender-row"><Avatar message={selectedMail} size="lg" /><div className="sender-copy"><div><strong>{selectedMail.senderName}</strong> <span>&lt;{selectedMail.from}&gt;</span></div><div className="recipient">收件人： {selectedMailAccount?.label ?? '当前账户'} &lt;{selectedMailAccount?.email ?? '—'}&gt;</div></div><time>{selectedMail.timestamp}<br /><span>今天</span></time><TooltipButton label="发件人更多信息"><Icon name="more" size={19} /></TooltipButton></div>
            <div className="message-content">
              {selectedMail.hasHtml && <div className="content-mode-row"><span>此邮件包含富文本内容{!remoteImagesEnabled && ' · 远程图片已屏蔽'}</span><button type="button" className="text-action" onClick={() => setHtmlMode((value) => !value)}>{isHtmlMode ? '查看纯文本' : '渲染 HTML'} <Icon name="grid" size={14} /></button></div>}
              {isHtmlMode && selectedMail.hasHtml ? <div className="html-rendered" dangerouslySetInnerHTML={{ __html: sanitizeHtml(selectedMail.htmlBody ?? initialHtml, remoteImagesEnabled) }} /> : selectedMail.body.map((paragraph) => <p key={paragraph}>{paragraph}</p>)}
            </div>
            {selectedMail.attachments && <div className="attachments"><div className="attachments-heading"><span><Icon name="paperclip" size={20} /> {selectedMail.attachments.length} 个附件</span><div><button type="button" onClick={() => { void Promise.all(selectedMail.attachments?.map(downloadAttachment) ?? []) }}><Icon name="download" size={17} /> 全部下载</button><button type="button" onClick={() => pushToast('正在保存到本地缓存', 'success')}><Icon name="cloud" size={17} /> 保存到云盘</button></div></div><div className="attachment-grid">{selectedMail.attachments.map((attachment) => { const progress = attachmentProgress[attachment.id]; return <button type="button" className="attachment-card" key={attachment.id} onClick={() => { if (progress != null) cancelAttachment(attachment.id); else void downloadAttachment(attachment) }}><span className={`file-glyph file-${attachment.kind}`}>{attachment.kind === 'pdf' ? 'PDF' : attachment.kind === 'sheet' ? 'X' : 'FILE'}</span><span className="attachment-copy"><strong>{attachment.name}</strong><small>{progress != null ? `${progress}% · 点击取消` : attachment.size}</small></span><Icon name={progress != null ? 'close' : 'download'} size={17} /></button> })}</div></div>}
            <div className="reply-composer"><Avatar message={{ ...selectedMail, avatar: 'OC', accent: '#2a5596' }} size="sm" /><div className="reply-input" onClick={() => openCompose()}>点击回复，或按 R 快速回复<div className="reply-tools"><span><Icon name="paperclip" size={19} /></span><span><Icon name="image" size={19} /></span><span className="reply-emoji">☺</span><span className="reply-a">A</span><button type="button" onClick={(event) => { event.stopPropagation(); openCompose() }}>回复 <span>⌄</span></button></div></div></div>
          </div>
        </section>

        <AnimatePresence initial={false}>
          {isAuthPanelOpen && <motion.aside className="auth-panel" initial={prefersReducedMotion ? false : { x: 24, opacity: 0 }} animate={{ x: 0, opacity: 1 }} exit={{ x: 24, opacity: 0 }} transition={{ duration: 0.24 }}>
            <div className="auth-panel-header"><div><Icon name="key" size={20} /><strong>授权码助手</strong></div><TooltipButton label="关闭授权码助手" onClick={() => setAuthPanelOpen(false)}><Icon name="close" size={18} /></TooltipButton></div>
            <div className="auth-tabs"><button type="button" className="is-active"><Icon name="lock" size={16} />授权码</button><button type="button" onClick={openNewAccount}><Icon name="settings" size={16} />设置</button></div>
            <div className="auth-card"><div className="auth-illustration"><Icon name="shieldCheck" size={40} /></div><h2>快速获取授权码</h2><p>用于第三方服务登录验证</p><button className="gradient-button" type="button" onClick={openNewAccount}><Icon name="copy" size={17} />管理授权码</button><div className="auth-validity"><Icon name="clock" size={16} />授权码仅保存在本机安全存储</div></div>
            <div className="auth-panel-section"><div className="panel-section-title">账户</div>{accounts.map((account) => <button type="button" className="auth-account-row" key={account.id} onClick={() => { setEditingAccountId(account.id); changeProvider(account.provider); setAccountEmail(account.email); setCustomImapHost(account.imapHost ?? 'imap.example.com'); setCustomImapPort(String(account.imapPort ?? 993)); setCustomImapSecurity(account.imapSecurity ?? 'tls'); setCustomSmtpHost(account.smtpHost ?? 'smtp.example.com'); setCustomSmtpPort(String(account.smtpPort ?? 465)); setCustomSmtpSecurity(account.smtpSecurity ?? 'tls'); setCustomAuthentication(account.authentication ?? (account.provider === 'outlook' ? 'oauth2' : 'app-password')); setAccountModalOpen(true) }}><ProviderMark provider={account.provider} size="sm" /><span><strong>{account.label}</strong><small>{account.email}</small></span><span className="auth-chevron">›</span></button>)}</div>
            <div className="auth-note"><Icon name="info" size={18} /><span>授权码仅用于登录验证<br />不会存储或同步到云端</span></div>
            <div className="auth-panel-foot"><button type="button" onClick={handleOpenProvider}><Icon name="link" size={15} />打开 {selectedProvider.label} 设置</button></div>
          </motion.aside>}
        </AnimatePresence>
        {!isAuthPanelOpen && <button className="auth-panel-reopen" type="button" onClick={() => setAuthPanelOpen(true)}><Icon name="key" size={18} />授权码助手</button>}
      </div>

      <AnimatePresence>
        {isSettingsOpen && <motion.div className="settings-popover" initial={{ opacity: 0, y: 8 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0, y: 8 }}><div className="settings-title"><span><Icon name="settings" size={17} />偏好设置</span><TooltipButton label="关闭设置" onClick={() => setSettingsOpen(false)}><Icon name="close" size={17} /></TooltipButton></div><div className="settings-row"><span><Icon name={theme === 'dark' ? 'moon' : 'theme'} size={17} /><span>外观主题<small>{theme === 'dark' ? '深色 · 午夜蓝' : '浅色 · 雪白'}</small></span></span><button type="button" className="theme-switch" onClick={() => setTheme((value) => value === 'dark' ? 'light' : 'dark')}><span className={theme === 'light' ? 'is-light' : ''}>{theme === 'dark' ? '深' : '浅'}</span></button></div><label className="settings-row css-row"><span><Icon name="brush" size={17} /><span>用户 CSS<small>可覆盖 MailGo 视觉变量</small></span></span><textarea value={customCss} onChange={(event) => setCustomCss(event.target.value)} placeholder="例如：:root { --accent: #ff6b8a; }" /></label><div className="settings-row"><span><Icon name="cloud" size={17} /><span>关闭时后台运行<small>最小化到系统托盘并继续同步</small></span></span><button type="button" className={`toggle-switch ${minimizeToTray ? 'is-on' : ''}`} onClick={() => { const next = !minimizeToTray; setMinimizeToTray(next); void invoke('app.set_minimize_to_tray', { enabled: next }).catch(() => undefined) }}><span /></button></div><div className="settings-row"><span><Icon name="image" size={17} /><span>加载远程图片<small>{remoteImagesEnabled ? '已允许 HTTPS 图片，可能包含追踪像素' : '默认屏蔽，保护隐私；CID 内嵌图片不受影响'}</small></span></span><button type="button" aria-label="加载远程图片" className={`toggle-switch ${remoteImagesEnabled ? 'is-on' : ''}`} onClick={() => { const next = !remoteImagesEnabled; setRemoteImagesEnabled(next); void invoke('app.set_remote_images', { enabled: next }).catch(() => undefined) }}><span /></button></div><div className="settings-row"><span><Icon name="bell" size={17} /><span>后台新邮件提醒<small>窗口隐藏时发送 Windows 托盘通知</small></span></span><button type="button" aria-label="后台新邮件提醒" className={`toggle-switch ${notificationsEnabled ? 'is-on' : ''}`} onClick={() => { const next = !notificationsEnabled; setNotificationsEnabled(next); void invoke('app.set_notifications', { enabled: next }).catch(() => undefined) }}><span /></button></div><div className="settings-actions"><button type="button" onClick={exportAccounts}><Icon name="download" size={16} />导出脱敏配置</button><button type="button" onClick={() => importInputRef.current?.click()} disabled={isImporting}><Icon name="folder" size={16} />{isImporting ? '导入中…' : '导入脱敏配置'}</button><button type="button" className="secure-transfer-button" onClick={openEncryptedExport} disabled={!isNativeRuntime || isImporting}><Icon name="shieldCheck" size={16} />导出加密配置</button><button type="button" className="secure-transfer-button" onClick={openEncryptedImport} disabled={!isNativeRuntime || isImporting}><Icon name="key" size={16} />导入加密配置</button></div><input ref={importInputRef} type="file" accept="application/json,.json" hidden onChange={importAccounts} /><input ref={encryptedTransferInputRef} type="file" accept="application/json,.json" hidden onChange={selectEncryptedTransferFile} /></motion.div>}
      </AnimatePresence>

      <AnimatePresence>{isHelpOpen && <HelpModal onClose={() => setHelpOpen(false)} />}</AnimatePresence>
      <AnimatePresence>{isComposeOpen && <ComposeModal accountId={selectedAccountId ?? accounts[0]?.id} draftId={composeDraftId} onDraftChanged={handleDraftChanged} onDraftRemoved={handleDraftRemoved} onClose={() => { setComposeOpen(false); setComposeDraftId(undefined); void refreshNativeDrafts() }} onSent={() => { setComposeOpen(false); setComposeDraftId(undefined); void refreshNativeDrafts(); pushToast('邮件已发送', 'success') }} onError={(message) => pushToast(message, 'error')} />}</AnimatePresence>
      <AnimatePresence>{isAccountModalOpen && <AccountModal editingAccountId={editingAccountId} provider={provider} setProvider={changeProvider} providerDefinition={selectedProvider} accountEmail={accountEmail} setAccountEmail={setAccountEmail} authorizationCode={authorizationCode} setAuthorizationCode={setAuthorizationCode} showAuthorizationCode={showAuthorizationCode} setShowAuthorizationCode={setShowAuthorizationCode} customImapHost={customImapHost} setCustomImapHost={setCustomImapHost} customImapPort={customImapPort} setCustomImapPort={setCustomImapPort} customImapSecurity={customImapSecurity} setCustomImapSecurity={setCustomImapSecurity} customSmtpHost={customSmtpHost} setCustomSmtpHost={setCustomSmtpHost} customSmtpPort={customSmtpPort} setCustomSmtpPort={setCustomSmtpPort} customSmtpSecurity={customSmtpSecurity} setCustomSmtpSecurity={setCustomSmtpSecurity} customAuthentication={customAuthentication} setCustomAuthentication={setCustomAuthentication} deviceFlow={deviceFlow} onClose={() => setAccountModalOpen(false)} onOpenProvider={() => { void handleOpenProvider() }} onCopy={handleCopy} onAdd={handleAddAccount} onRemove={handleRemoveAccount} />}</AnimatePresence>
      <AnimatePresence>{transferMode && <TransferModal mode={transferMode} fileName={transferFile?.name} isBusy={isImporting} onClose={() => { if (!isImporting) { setTransferMode(null); setTransferFile(null) } }} onSubmit={handleEncryptedTransfer} />}</AnimatePresence>
      <div className="toast-stack" aria-live="polite">{toasts.map((toast) => <motion.div key={toast.id} className={`toast toast-${toast.tone}`} initial={{ opacity: 0, y: 12, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 12 }}><Icon name={toast.tone === 'success' ? 'checkCircle' : toast.tone === 'error' ? 'info' : 'bell'} size={17} /><span>{toast.message}</span></motion.div>)}</div>
    </div>
  )
}

function TransferModal({ mode, fileName, isBusy, onClose, onSubmit }: { mode: TransferMode; fileName?: string; isBusy: boolean; onClose: () => void; onSubmit: (passphrase: string) => void }) {
  const [passphrase, setPassphrase] = useState('')
  const [showPassphrase, setShowPassphrase] = useState(false)
  const [validationError, setValidationError] = useState('')
  const isExport = mode === 'export-encrypted'

  const submit = (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (passphrase.length < 12) {
      setValidationError('请使用至少 12 个字符的转移密码')
      return
    }
    setValidationError('')
    onSubmit(passphrase)
  }

  return <motion.div className="modal-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget && !isBusy) onClose() }}><motion.div className="transfer-modal" initial={{ opacity: 0, y: 14, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 8, scale: 0.98 }} role="dialog" aria-modal="true" aria-labelledby="transfer-modal-title"><div className="modal-header"><div><Icon name="shieldCheck" size={21} /><h2 id="transfer-modal-title">{isExport ? '导出加密账户配置' : '导入加密账户配置'}</h2></div><TooltipButton label="关闭" onClick={onClose}><Icon name="close" size={18} /></TooltipButton></div><form className="transfer-modal-body" onSubmit={submit}>{!isExport && <div className="transfer-file"><Icon name="folder" size={18} /><span><strong>{fileName || '已选择配置文件'}</strong><small>凭据将在本机解密并写入 Windows Credential Manager</small></span></div>}<p>{isExport ? '账户元数据与 Windows Credential Manager 中的凭据会打包为加密文件，可迁移到另一台 MailGo。' : '请输入创建加密文件时使用的转移密码。解密成功后，账户会写入本机安全存储。'}</p><label>转移密码<span className="secret-input"><input autoFocus required minLength={12} type={showPassphrase ? 'text' : 'password'} value={passphrase} onChange={(event) => { setPassphrase(event.target.value); setValidationError('') }} placeholder="至少 12 个字符" autoComplete="new-password" /><button type="button" onClick={() => setShowPassphrase((value) => !value)} aria-label={showPassphrase ? '隐藏转移密码' : '显示转移密码'}><Icon name={showPassphrase ? 'eyeSlash' : 'eye'} size={17} /></button></span></label>{validationError && <div className="transfer-error" role="alert"><Icon name="info" size={15} />{validationError}</div>}<div className="transfer-warning"><Icon name="shieldCheck" size={16} /><span>密码不会保存到 MailGo。忘记密码无法恢复加密包；请使用可信位置保存文件。</span></div><div className="modal-footer"><span><Icon name="key" size={17} />Argon2id + ChaCha20-Poly1305</span><div><button className="secondary-button" type="button" onClick={onClose} disabled={isBusy}>取消</button><button className="gradient-button" type="submit" disabled={isBusy}>{isBusy ? '处理中…' : (isExport ? '生成加密包' : '解密并导入')}<Icon name={isExport ? 'download' : 'folder'} size={17} /></button></div></div></form></motion.div></motion.div>
}

function HelpModal({ onClose }: { onClose: () => void }) {
  return <motion.div className="modal-backdrop help-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}><motion.div className="help-modal" initial={{ opacity: 0, y: 14, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 8, scale: 0.98 }} role="dialog" aria-modal="true" aria-labelledby="help-title"><div className="modal-header"><div><Icon name="help" size={21} /><h2 id="help-title">MailGo 帮助中心</h2></div><TooltipButton label="关闭帮助中心" onClick={onClose}><Icon name="close" size={19} /></TooltipButton></div><div className="help-modal-body"><section><h3>常用快捷键</h3><div className="shortcut-row"><span>写邮件</span><kbd>C</kbd></div><div className="shortcut-row"><span>聚焦搜索</span><kbd>Ctrl K</kbd></div><div className="shortcut-row"><span>回复当前邮件</span><kbd>R</kbd></div><div className="shortcut-row"><span>关闭弹窗</span><kbd>Esc</kbd></div></section><section><h3>账户与同步</h3><p>Google 与 Outlook 优先使用 OAuth 安全授权；QQ 邮箱和自定义 IMAP/SMTP 使用服务商生成的授权码或应用密码。</p><p>同步失败时，邮件仍保留在本地缓存；离线状态下的归档、删除和已读操作会在恢复连接后重放。</p></section><section><h3>隐私与安全</h3><p>授权凭据只交给本机安全存储。普通导出不包含授权码；Windows 桌面端可使用加密账户迁移包转移完整配置。HTML 邮件默认屏蔽远程图片，避免追踪像素；需要时可在设置中显式开启。</p></section></div><div className="modal-footer"><span><Icon name="shieldCheck" size={17} />MailGo 运行在本机，数据由你控制</span><button className="gradient-button" type="button" onClick={onClose}>知道了</button></div></motion.div></motion.div>
}

function AccountModal({ editingAccountId, provider, setProvider, providerDefinition, accountEmail, setAccountEmail, authorizationCode, setAuthorizationCode, showAuthorizationCode, setShowAuthorizationCode, customImapHost, setCustomImapHost, customImapPort, setCustomImapPort, customImapSecurity, setCustomImapSecurity, customSmtpHost, setCustomSmtpHost, customSmtpPort, setCustomSmtpPort, customSmtpSecurity, setCustomSmtpSecurity, customAuthentication, setCustomAuthentication, deviceFlow, onClose, onOpenProvider, onCopy, onAdd, onRemove }: { editingAccountId: string | null; provider: Provider; setProvider: (provider: Provider) => void; providerDefinition: ReturnType<typeof providerFor>; accountEmail: string; setAccountEmail: (value: string) => void; authorizationCode: string; setAuthorizationCode: (value: string) => void; showAuthorizationCode: boolean; setShowAuthorizationCode: (value: boolean) => void; customImapHost: string; setCustomImapHost: (value: string) => void; customImapPort: string; setCustomImapPort: (value: string) => void; customImapSecurity: string; setCustomImapSecurity: (value: string) => void; customSmtpHost: string; setCustomSmtpHost: (value: string) => void; customSmtpPort: string; setCustomSmtpPort: (value: string) => void; customSmtpSecurity: string; setCustomSmtpSecurity: (value: string) => void; customAuthentication: string; setCustomAuthentication: (value: string) => void; deviceFlow: DeviceFlowState | null; onClose: () => void; onOpenProvider: () => void; onCopy: () => void; onAdd: () => void; onRemove: () => void }) {
  const isOAuth = customAuthentication === 'oauth2'
  const credentialLabel = isOAuth ? '手动授权码（可选）' : providerDefinition.requiresAuthCode ? '授权码' : '登录凭据'
  const guideTitle = isOAuth ? '如何完成安全授权？' : '如何获取授权码？'
  const guide = isOAuth
    ? provider === 'outlook'
      ? ['打开 Microsoft 设备验证页面', '输入 MailGo 显示的设备代码', '完成账户授权后返回 MailGo']
      : ['点击开始授权并打开服务商登录页', '在服务商页面确认 MailGo 的访问权限', '完成后返回 MailGo，系统会自动保存令牌']
    : providerDefinition.guide
  return <motion.div className="modal-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}><motion.div className="account-modal" initial={{ opacity: 0, y: 14, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 8, scale: 0.98 }} role="dialog" aria-modal="true" aria-labelledby="account-modal-title"><div className="modal-header"><div><Icon name="user" size={21} /><h2 id="account-modal-title">{editingAccountId ? '重新授权账户' : '添加账户'}</h2></div><TooltipButton label="关闭" onClick={onClose}><Icon name="close" size={19} /></TooltipButton></div><div className="account-modal-body"><div className="provider-chooser">{providerDefinitions.map((item) => <button key={item.id} type="button" className={`provider-option ${provider === item.id ? 'is-selected' : ''}`} onClick={() => setProvider(item.id)}><ProviderMark provider={item.id} size="md" /><span><strong>{item.label}</strong><small>{item.description}</small></span>{provider === item.id && <Icon name="checkCircle" size={19} />}</button>)}</div><div className="account-form"><label>邮箱地址<input type="email" value={accountEmail} onChange={(event) => setAccountEmail(event.target.value)} placeholder={provider === 'qq' ? 'yourname@qq.com' : 'name@example.com'} autoFocus /></label>{(provider === 'google' || provider === 'outlook') && <label>认证方式<select value={customAuthentication} onChange={(event) => setCustomAuthentication(event.target.value)}><option value="oauth2">OAuth2 安全授权</option>{provider === 'google' && <option value="app-password">应用专用密码</option>}</select></label>}<label><span className="label-with-action">{credentialLabel}<button type="button" onClick={onOpenProvider}>{isOAuth ? '打开授权页面' : '如何获取授权码？'} <Icon name="link" size={13} /></button></span><span className="secret-input"><input type={showAuthorizationCode ? 'text' : 'password'} value={authorizationCode} onChange={(event) => setAuthorizationCode(event.target.value)} placeholder={isOAuth ? 'OAuth 授权完成后无需粘贴' : '粘贴邮箱授权码'} /><button type="button" onClick={() => setShowAuthorizationCode(!showAuthorizationCode)} aria-label={showAuthorizationCode ? '隐藏授权码' : '显示授权码'}><Icon name={showAuthorizationCode ? 'eyeSlash' : 'eye'} size={17} /></button><button type="button" onClick={onCopy} aria-label="复制授权码"><Icon name="copy" size={17} /></button></span></label>{deviceFlow && <div className="device-flow-box"><div className="device-flow-heading"><span><Icon name="shieldCheck" size={16} />Outlook 设备授权</span><strong>{deviceFlow.status === 'complete' ? '已完成' : deviceFlow.status === 'error' ? '需要重试' : '等待验证'}</strong></div><code>{deviceFlow.userCode}</code><p>{deviceFlow.status === 'complete' ? '设备验证已完成，可以开始同步。' : (deviceFlow.message || '请打开验证页完成 Microsoft 账户授权。')}</p><small>{deviceFlow.verificationUri}</small>{deviceFlow.status !== 'complete' && <button type="button" onClick={onOpenProvider}>{deviceFlow.status === 'error' ? '重新开始授权' : '重新打开验证页'} <Icon name="link" size={13} /></button>}</div>}{provider === 'other' && <div className="custom-transport-fields"><div className="transport-heading"><Icon name="settings" size={15} />自定义服务器</div><div className="transport-row"><label>IMAP 主机<input value={customImapHost} onChange={(event) => setCustomImapHost(event.target.value)} placeholder="imap.example.com" /></label><label>端口<input type="number" min="1" max="65535" value={customImapPort} onChange={(event) => setCustomImapPort(event.target.value)} /></label><label>安全<input value={customImapSecurity} onChange={(event) => setCustomImapSecurity(event.target.value)} placeholder="tls / starttls" /></label></div><div className="transport-row"><label>SMTP 主机<input value={customSmtpHost} onChange={(event) => setCustomSmtpHost(event.target.value)} placeholder="smtp.example.com" /></label><label>端口<input type="number" min="1" max="65535" value={customSmtpPort} onChange={(event) => setCustomSmtpPort(event.target.value)} /></label><label>安全<input value={customSmtpSecurity} onChange={(event) => setCustomSmtpSecurity(event.target.value)} placeholder="tls / starttls" /></label></div><label>认证方式<select value={customAuthentication} onChange={(event) => setCustomAuthentication(event.target.value)}><option value="password">密码 / 授权码</option><option value="app-password">应用专用密码</option><option value="oauth2">OAuth2 Bearer Token</option></select></label></div>}<div className="guide-box"><div className="guide-heading"><span><Icon name="key" size={17} />{guideTitle}</span><em>{providerDefinition.label}</em></div>{guide.map((step, index) => <div className="guide-step" key={step}><span className="step-number">{index + 1}</span><span><strong>{step}</strong><small>{isOAuth ? (index === 0 ? 'MailGo 会在本机发起安全授权流程' : index === 1 ? '只授予邮件同步所需的账户权限' : '令牌仅保存到本机系统安全存储') : (index === 0 ? `登录 ${providerDefinition.label}，打开设置页面` : index === 1 ? '找到第三方客户端或账户安全选项' : '复制生成的授权凭据，返回此处粘贴')}</small></span>{index === 0 && <button type="button" onClick={onOpenProvider}>{isOAuth ? '开始授权' : '前往设置'} <Icon name="link" size={13} /></button>}</div>)}</div></div></div><div className="modal-footer"><span><Icon name="shieldCheck" size={17} />凭据只保存在本机，不会上传到第三方</span><div>{editingAccountId && <button className="danger-button" type="button" onClick={onRemove}><Icon name="trash" size={16} />移除账户</button>}<button className="secondary-button" type="button" onClick={onClose}>取消</button><button className="gradient-button" type="button" onClick={onAdd}><Icon name="rotate" size={17} />{editingAccountId ? '保存并同步' : '开始同步'}</button></div></div></motion.div></motion.div>
}

function ComposeModal({ accountId, draftId: openDraftId, onClose, onSent, onError, onDraftChanged, onDraftRemoved }: { accountId?: string; draftId?: string; onClose: () => void; onSent: () => void; onError: (message: string) => void; onDraftChanged?: (draft: NativeDraft) => void; onDraftRemoved?: (draftId: string) => void }) {
  const [to, setTo] = useState('')
  const [cc, setCc] = useState('')
  const [bcc, setBcc] = useState('')
  const [showCopyFields, setShowCopyFields] = useState(false)
  const [subject, setSubject] = useState('')
  const [body, setBody] = useState('')
  const [htmlMode, setHtmlMode] = useState(false)
  const [draftId, setDraftId] = useState<string | undefined>()
  const [draftStatus, setDraftStatus] = useState('')
  const [draftReady, setDraftReady] = useState(false)
  const [attachments, setAttachments] = useState<File[]>([])
  const [isSending, setSending] = useState(false)
  const [uploadingName, setUploadingName] = useState('')
  const fileInputRef = useRef<HTMLInputElement>(null)
  const isNativeRuntime = Boolean(window.ipc?.postMessage)
  const maxAttachmentBytes = 25 * 1024 * 1024
  const maxTotalAttachmentBytes = 50 * 1024 * 1024

  useEffect(() => {
    let cancelled = false
    setDraftReady(false)
    setDraftStatus('')
    if (!isNativeRuntime || !accountId) {
      setDraftReady(true)
      return () => { cancelled = true }
    }
    void invoke<NativeDraft[]>('drafts.list', { accountId }, 30_000).then((drafts) => {
      const draft = openDraftId ? drafts.find((item) => item.id === openDraftId) : drafts[0]
      if (cancelled || !draft) return
      setDraftId(draft.id)
      setTo(draft.to)
      setCc(draft.cc)
      setBcc(draft.bcc)
      setShowCopyFields(Boolean(draft.cc || draft.bcc))
      setSubject(draft.subject)
      setBody(draft.body)
      setHtmlMode(draft.htmlMode)
      setDraftStatus(openDraftId ? '已恢复草稿' : '已恢复最近草稿')
      onDraftChanged?.(draft)
    }).catch(() => undefined).finally(() => {
      if (!cancelled) setDraftReady(true)
    })
    return () => { cancelled = true }
  }, [accountId, isNativeRuntime, onDraftChanged, openDraftId])

  useEffect(() => {
    if (!isNativeRuntime || !accountId || !draftReady || isSending) return
    if (![to, cc, bcc, subject, body].some((value) => value.trim())) return
    const timer = window.setTimeout(() => {
      void invoke<NativeDraft>('drafts.save', {
        ...(draftId ? { id: draftId } : {}),
        accountId,
        to,
        cc,
        bcc,
        subject,
        body,
        htmlMode,
      }, 30_000).then((draft) => {
        setDraftId(draft.id)
        setDraftStatus('草稿已自动保存')
      }).catch(() => setDraftStatus('草稿保存失败，将在下次输入时重试'))
    }, 700)
    return () => window.clearTimeout(timer)
  }, [accountId, bcc, body, cc, draftId, draftReady, htmlMode, isNativeRuntime, isSending, subject, to])

  const addFiles = (event: React.ChangeEvent<HTMLInputElement>) => {
    const incoming = Array.from(event.target.files ?? [])
    event.target.value = ''
    let total = attachments.reduce((sum, file) => sum + file.size, 0)
    const accepted: File[] = []
    for (const file of incoming) {
      if (attachments.length + accepted.length >= 10) {
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
      accepted.push(file)
      total += file.size
    }
    if (accepted.length) setAttachments((current) => [...current, ...accepted])
  }

  const uploadAttachment = async (file: File) => {
    const start = await invoke<NativeAttachmentUploadStartResponse>('mail.attachment.upload.start', {
      fileName: file.name,
      contentType: file.type || 'application/octet-stream',
      size: file.size,
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

  const discard = async () => {
    if (!draftId) {
      onClose()
      return
    }
    setSending(true)
    try {
      if (isNativeRuntime && accountId) {
        await invoke('drafts.remove', { accountId, id: draftId }, 30_000)
        onDraftRemoved?.(draftId)
      }
      onClose()
    } catch (error) {
      onError(error instanceof Error ? error.message : '草稿丢弃失败，请稍后重试')
    } finally {
      setSending(false)
    }
  }

  const send = async () => {
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
    try {
      if (isNativeRuntime) {
        for (const file of attachments) {
          setUploadingName('正在上传 ' + file.name)
          uploadIds.push(await uploadAttachment(file))
        }
        await invoke('mail.send', {
          accountId,
          to: to.trim(),
          ...(cc.trim() ? { cc: cc.trim() } : {}),
          ...(bcc.trim() ? { bcc: bcc.trim() } : {}),
          subject: subject.trim() || '(无主题)',
          textBody: body,
          ...(htmlMode && body.trim() ? { htmlBody: `<div>${escapeHtml(body).replace(/\r?\n/g, '<br>')}</div>` } : {}),
          ...(uploadIds.length ? { attachmentIds: uploadIds } : {}),
        })
      } else {
        await new Promise((resolve) => window.setTimeout(resolve, 700))
      }
      if (isNativeRuntime && accountId && draftId) {
        await invoke('drafts.remove', { accountId, id: draftId }, 30_000).catch(() => undefined)
        onDraftRemoved?.(draftId)
      }
      onSent()
    } catch (error) {
      await Promise.all(uploadIds.map((uploadId) => invoke('mail.attachment.upload.cancel', { uploadId }).catch(() => undefined)))
      onError(error instanceof Error ? error.message : '邮件发送失败，请稍后重试')
    } finally {
      setUploadingName('')
      setSending(false)
    }
  }

  return <motion.div className="modal-backdrop compose-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}><motion.div className="compose-modal" initial={{ opacity: 0, y: 20 }} animate={{ opacity: 1 }} exit={{ opacity: 0, y: 20 }}><div className="compose-header"><strong>新邮件</strong><div><TooltipButton label="最小化撰写窗口"><span className="window-minimize" /></TooltipButton><TooltipButton label="关闭撰写窗口" onClick={onClose}><Icon name="close" size={17} /></TooltipButton></div></div><div className="compose-recipient-row"><label>收件人<input autoFocus value={to} onChange={(event) => setTo(event.target.value)} placeholder="name@example.com，可用逗号分隔多个地址" /></label><button type="button" className="copy-fields-button" onClick={() => setShowCopyFields((value) => !value)} aria-expanded={showCopyFields}>{showCopyFields ? '隐藏抄送' : '抄送 / 密送'}</button></div>{showCopyFields && <><label>抄送<input value={cc} onChange={(event) => setCc(event.target.value)} placeholder="可选，多个地址用逗号分隔" /></label><label>密送<input value={bcc} onChange={(event) => setBcc(event.target.value)} placeholder="可选，多个地址用逗号分隔" /></label></>}<label>主题<input value={subject} onChange={(event) => setSubject(event.target.value)} placeholder="主题" /></label><textarea className="compose-body" value={body} onChange={(event) => setBody(event.target.value)} placeholder={htmlMode ? '输入内容，将以 HTML + 纯文本双格式发送…' : '写下你的邮件…'} />{draftStatus && <div className="compose-draft-status" aria-live="polite"><Icon name="cloud" size={14} />{draftStatus}</div>}{htmlMode && <div className="compose-format-note"><Icon name="grid" size={14} />此邮件将附带安全的 HTML 版本，同时保留纯文本版本</div>}{attachments.length > 0 && <div className="compose-attachments" aria-label="待发送附件">{attachments.map((file, index) => <div className="compose-attachment" key={file.name + '-' + file.size + '-' + index}><span>{file.name}</span><small>{Math.max(1, Math.round(file.size / 1024))} KB</small><button type="button" onClick={() => setAttachments((current) => current.filter((_, itemIndex) => itemIndex !== index))} aria-label={'移除附件 ' + file.name}>×</button></div>)}</div>}{uploadingName && <div className="compose-uploading" aria-live="polite"><Icon name="rotate" size={14} />{uploadingName}</div>}<div className="compose-footer"><div><TooltipButton label="添加附件" onClick={() => fileInputRef.current?.click()}><Icon name="paperclip" size={19} /></TooltipButton><input ref={fileInputRef} type="file" multiple hidden onChange={addFiles} /><TooltipButton label="插入图片" onClick={() => fileInputRef.current?.click()}><Icon name="image" size={19} /></TooltipButton><TooltipButton label={htmlMode ? '关闭 HTML 格式' : '启用 HTML 格式'} active={htmlMode} onClick={() => setHtmlMode((value) => !value)}><span className="reply-a">A</span></TooltipButton></div><div className="compose-send-actions">{draftId && <button type="button" className="danger-button" onClick={() => { void discard() }} disabled={isSending}><Icon name="trash" size={16} />丢弃草稿</button>}<button type="button" className="gradient-button" onClick={send} disabled={isSending}>{isSending ? (uploadingName || '发送中…') : '发送'}<Icon name="send" size={17} /></button></div></div></motion.div></motion.div>
}

export default App
