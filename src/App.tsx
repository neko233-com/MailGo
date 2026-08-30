import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { useEffect, useMemo, useRef, useState } from 'react'
import { Icon, type IconName } from './components/Icon'
import { folderLabels, providerDefinitions, sampleAccounts, sampleMails } from './data'
import { invoke, readNativeState } from './lib/ipc'
import type { FolderId, MailAccount, MailAttachment, MailMessage, NativeAttachmentResponse, NativeCachedMessage, NativeMailboxResponse, NativeMessageResponse, NativeSyncItem, NativeSyncResponse, Provider, SmartCategory, ThemeMode } from './types'

type ToastTone = 'info' | 'success' | 'error'
type Toast = { id: number; message: string; tone: ToastTone }

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

function formatCount(value: number) {
  return value > 99 ? '99+' : String(value)
}

function sanitizeHtml(input: string) {
  const documentParser = new DOMParser().parseFromString(input, 'text/html')
  documentParser.querySelectorAll('script, iframe, object, embed, form, link, meta, style').forEach((node) => node.remove())
  documentParser.querySelectorAll('*').forEach((node) => {
    Array.from(node.attributes).forEach((attribute) => {
      if (attribute.name.toLowerCase().startsWith('on')) node.removeAttribute(attribute.name)
      if (['href', 'src', 'action'].includes(attribute.name.toLowerCase()) && attribute.value.toLowerCase().startsWith('javascript:')) {
        node.removeAttribute(attribute.name)
      }
    })
  })
  return documentParser.body.innerHTML
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

function loadTheme(): ThemeMode {
  const stored = window.localStorage.getItem('mailgo-theme')
  return stored === 'light' ? 'light' : 'dark'
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

function TooltipButton({ label, onClick, children, active = false, className = '' }: { label: string; onClick?: () => void; children: React.ReactNode; active?: boolean; className?: string }) {
  return (
    <button className={`icon-button ${active ? 'is-active' : ''} ${className}`} onClick={onClick} aria-label={label} title={label} type="button">
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
  const [query, setQuery] = useState('')
  const [filterUnread, setFilterUnread] = useState(false)
  const [isComposeOpen, setComposeOpen] = useState(false)
  const [isAccountModalOpen, setAccountModalOpen] = useState(false)
  const [isAuthPanelOpen, setAuthPanelOpen] = useState(true)
  const [isSettingsOpen, setSettingsOpen] = useState(false)
  const [isSyncing, setSyncing] = useState(false)
  const [isLoadingEarlier, setLoadingEarlier] = useState(false)
  const [mailboxMeta, setMailboxMeta] = useState<Record<string, { oldestUid?: number; hasMore?: boolean }>>({})
  const [isHtmlMode, setHtmlMode] = useState(false)
  const [isImporting, setImporting] = useState(false)
  const [toasts, setToasts] = useState<Toast[]>([])
  const [minimizeToTray, setMinimizeToTray] = useState(true)
  const [provider, setProvider] = useState<Provider>('qq')
  const [accountEmail, setAccountEmail] = useState('')
  const [authorizationCode, setAuthorizationCode] = useState('')
  const [oauthSessionId, setOauthSessionId] = useState('')
  const [showAuthorizationCode, setShowAuthorizationCode] = useState(false)
  const [customCss, setCustomCss] = useState(() => window.localStorage.getItem('mailgo-custom-css') ?? '')
  const [customImapHost, setCustomImapHost] = useState('imap.example.com')
  const [customImapPort, setCustomImapPort] = useState('993')
  const [customImapSecurity, setCustomImapSecurity] = useState('tls')
  const [customSmtpHost, setCustomSmtpHost] = useState('smtp.example.com')
  const [customSmtpPort, setCustomSmtpPort] = useState('465')
  const [customSmtpSecurity, setCustomSmtpSecurity] = useState('tls')
  const [customAuthentication, setCustomAuthentication] = useState('password')
  const importInputRef = useRef<HTMLInputElement>(null)
  const isNativeRuntime = Boolean(window.ipc?.postMessage)

  const pushToast = (message: string, tone: ToastTone = 'info') => {
    const id = Date.now() + Math.random()
    setToasts((current) => [...current.slice(-2), { id, message, tone }])
    window.setTimeout(() => setToasts((current) => current.filter((toast) => toast.id !== id)), 3600)
  }

  const changeProvider = (nextProvider: Provider) => {
    setProvider(nextProvider)
    setCustomAuthentication(nextProvider === 'outlook' ? 'oauth2' : nextProvider === 'other' ? 'password' : 'app-password')
    setOauthSessionId('')
  }

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
    const handleShortcut = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
        event.preventDefault()
        document.getElementById('mail-search')?.focus()
      }
      if (event.key === 'Escape') {
        setComposeOpen(false)
        setAccountModalOpen(false)
      }
    }
    window.addEventListener('keydown', handleShortcut)
    return () => window.removeEventListener('keydown', handleShortcut)
  }, [])

  const selectedMail = mails.find((mail) => mail.id === selectedMailId) ?? mails[0]
  const selectedProvider = providerFor(provider)
  const visibleMails = useMemo(() => {
    const lowerQuery = query.trim().toLowerCase()
    return mails.filter((mail) => {
      const folderMatch = selectedFolder === 'starred' ? mail.starred : mail.folder === selectedFolder
      const accountMatch = !selectedAccountId || mail.accountId === selectedAccountId
      const categoryMatch = !selectedCategory || mail.category === selectedCategory
      const unreadMatch = !filterUnread || mail.unread
      const queryMatch = !lowerQuery || `${mail.senderName} ${mail.subject} ${mail.preview}`.toLowerCase().includes(lowerQuery)
      return folderMatch && accountMatch && categoryMatch && unreadMatch && queryMatch
    })
  }, [filterUnread, mails, query, selectedAccountId, selectedCategory, selectedFolder])

  const groupedMails = useMemo(() => {
    return visibleMails.reduce<Record<string, MailMessage[]>>((groups, mail) => {
      groups[mail.dateGroup] ??= []
      groups[mail.dateGroup].push(mail)
      return groups
    }, {})
  }, [visibleMails])

  const canLoadEarlier = isNativeRuntime && selectedFolder !== 'starred' && accounts
    .filter((account) => !selectedAccountId || account.id === selectedAccountId)
    .some((account) => mailboxMeta[nativeMailboxKey(account.id, nativeFolderName(account, selectedFolder))]?.hasMore)

  const selectMail = async (mail: MailMessage) => {
    setSelectedMailId(mail.id)
    if (mail.unread) setMails((current) => current.map((item) => item.id === mail.id ? { ...item, unread: false } : item))
    if (isNativeRuntime && mail.nativeUid) {
      void invoke('mail.mark_read', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: false }).catch(() => undefined)
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
      void invoke('mail.star', { accountId: mail.accountId, folder: mail.nativeFolder ?? 'INBOX', uid: mail.nativeUid, enabled: nextStarred }).catch(() => pushToast('星标同步失败，可稍后重试', 'error'))
    }
    pushToast(nextStarred ? '已添加到星标' : '已移出星标', 'success')
  }

  const downloadAttachment = async (attachment: MailAttachment) => {
    if (!isNativeRuntime || selectedMail.nativeUid == null || attachment.nativeIndex == null) {
      pushToast(`${attachment.name} 已加入下载队列`, 'success')
      return
    }
    try {
      const response = await invoke<NativeAttachmentResponse>('mail.attachment', {
        accountId: selectedMail.accountId,
        folder: selectedMail.nativeFolder ?? 'INBOX',
        uid: selectedMail.nativeUid,
        index: attachment.nativeIndex,
      }, 60_000)
      const binary = Uint8Array.from(atob(response.dataBase64), (character) => character.charCodeAt(0))
      const url = URL.createObjectURL(new Blob([binary], { type: response.contentType }))
      const anchor = document.createElement('a')
      anchor.href = url
      anchor.download = response.fileName
      anchor.click()
      URL.revokeObjectURL(url)
      pushToast(`${response.fileName} 下载完成`, 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '附件读取失败，请先打开邮件正文', 'error')
    }
  }

  const selectFolder = (folder: FolderId) => {
    setSelectedFolder(folder)
    setSelectedCategory(null)
    setSelectedAccountId(null)
    const first = mails.find((mail) => folder === 'starred' ? mail.starred : mail.folder === folder)
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
        const flow = await invoke<{ sessionId: string; authorizationUrl: string }>('auth.start', {
          provider,
          email: accountEmail.trim(),
        })
        setOauthSessionId(flow.sessionId)
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
    const id = `${provider}-${Date.now()}`
    const newAccount: MailAccount = {
      id,
      provider,
      label: selectedProvider.label,
      email: accountEmail.trim(),
      unread: 0,
      accent: selectedProvider.accent,
      status: 'syncing',
      lastSync: '正在同步…',
    }
    setAccounts((current) => [...current, newAccount])
    try {
      await invoke('accounts.add', {
        id,
        provider,
        label: selectedProvider.label,
        email: accountEmail.trim(),
        authorizationCode,
        authentication: customAuthentication,
        ...(oauthSessionId ? { oauthSessionId } : {}),
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
    } catch {
      setAccounts((current) => current.filter((account) => account.id !== id))
      pushToast('账户添加失败，请检查授权码、OAuth 配置或服务器设置', 'error')
      return
    }
    setAuthorizationCode('')
    setOauthSessionId('')
    setAccountEmail('')
    setAccountModalOpen(false)
    setSelectedAccountId(id)
    pushToast(`${selectedProvider.label}账户已加入，正在同步邮件`, 'success')
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
      const parsed = JSON.parse(await file.text()) as { accounts?: MailAccount[]; schemaVersion?: number }
      if (parsed.schemaVersion !== 1 || !Array.isArray(parsed.accounts)) throw new Error('不支持的配置格式')
      const imported = parsed.accounts.filter((account) => account.id && account.email && account.provider).map((account) => ({ ...account, status: 'needs-auth' as const, lastSync: '等待重新授权' }))
      setAccounts((current) => [...current, ...imported])
      try { await invoke('accounts.import', { accounts: imported }) } catch { /* Browser preview fallback. */ }
      pushToast(`已导入 ${imported.length} 个账户，请逐一补充授权码`, 'success')
    } catch (error) {
      pushToast(error instanceof Error ? error.message : '配置导入失败', 'error')
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
            <button className="compose-button" type="button" onClick={() => setComposeOpen(true)}><Icon name="edit" size={19} /><span>写邮件</span><span className="compose-shortcut">C</span></button>
            <nav className="folder-nav" aria-label="邮件文件夹">
              {folderLabels.map((folder) => (
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
            <div className="section-label-row"><span>账户</span><TooltipButton label="添加账户" onClick={() => setAccountModalOpen(true)}><Icon name="add" size={16} /></TooltipButton></div>
            <div className="account-list">
              {accounts.map((account) => (
                <button key={account.id} type="button" className={`account-row ${selectedAccountId === account.id ? 'is-selected' : ''}`} onClick={() => { setSelectedAccountId(account.id); setSelectedCategory(null); setSelectedFolder('inbox') }}>
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
            <div className="storage-foot"><span><Icon name="cloud" size={13} /> 离线可查看最近邮件</span><button type="button" onClick={handleSync}><Icon name="rotate" size={13} /> {isSyncing ? '同步中…' : '立即同步'}</button></div>
          </div>

          <div className="sidebar-footer">
            <TooltipButton label="设置" active={isSettingsOpen} onClick={() => setSettingsOpen((value) => !value)}><Icon name="settings" size={19} /></TooltipButton>
            <TooltipButton label="帮助中心" onClick={() => pushToast('帮助中心即将上线', 'info')}><Icon name="help" size={19} /></TooltipButton>
            <TooltipButton label="收起侧栏" className="sidebar-collapse"><Icon name="menu" size={19} /></TooltipButton>
          </div>
        </aside>

        <main className="mail-list-panel">
          <div className="panel-toolbar">
            <div className="search-wrap"><Icon name="search" size={19} /><input id="mail-search" value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索邮件" aria-label="搜索邮件" /><kbd>Ctrl K</kbd></div>
            <button className={`filter-button ${filterUnread ? 'is-active' : ''}`} type="button" onClick={() => setFilterUnread((value) => !value)}><Icon name="filter" size={17} /> 筛选{filterUnread && <span className="filter-dot" />}</button>
          </div>
          <div className="list-toolbar">
            <label className="checkbox-wrap"><input type="checkbox" aria-label="选择所有邮件" /><span /></label>
            <button type="button" className="toolbar-action" onClick={() => pushToast('已将选中邮件归档', 'success')}><Icon name="archive" size={17} /> <span>归档</span></button>
            <button type="button" className="toolbar-action" onClick={() => pushToast('已将选中邮件删除', 'success')}><Icon name="trash" size={17} /> <span>删除</span></button>
            <button type="button" className="toolbar-action" onClick={() => pushToast('已标记为已读', 'success')}><Icon name="message" size={17} /> <span>标为已读</span></button>
            <TooltipButton label="更多操作" onClick={() => pushToast('更多批量操作即将上线', 'info')}><Icon name="more" size={18} /></TooltipButton>
          </div>
          <div className="mail-list-scroll">
            <AnimatePresence initial={false} mode="popLayout">
              {Object.entries(groupedMails).map(([group, mails]) => (
                <motion.section key={group} className="mail-group" initial={prefersReducedMotion ? false : { opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}>
                  <div className="mail-group-label">{group}</div>
                  {mails.map((mail) => (
                    <motion.div layout key={mail.id} className={`mail-row ${selectedMailId === mail.id ? 'is-selected' : ''} ${mail.unread ? 'is-unread' : ''}`} onClick={() => selectMail(mail)} whileHover={prefersReducedMotion ? undefined : { y: -1 }} transition={{ duration: 0.16 }}>
                      <label className="checkbox-wrap row-checkbox" onClick={(event) => event.stopPropagation()}><input type="checkbox" aria-label={`选择 ${mail.subject}`} /><span /></label>
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
            <div className="reading-actions"><TooltipButton label="回复" onClick={() => setComposeOpen(true)}><Icon name="reply" size={18} /></TooltipButton><span>回复</span><TooltipButton label="回复全部" onClick={() => setComposeOpen(true)}><Icon name="reply" size={18} /></TooltipButton><span>回复全部</span><TooltipButton label="转发" onClick={() => setComposeOpen(true)}><Icon name="forward" size={18} /></TooltipButton><span>转发</span><TooltipButton label="归档" onClick={() => pushToast('邮件已归档', 'success')}><Icon name="archive" size={18} /></TooltipButton><span>归档</span><TooltipButton label="删除" onClick={() => pushToast('邮件已移入回收站', 'success')}><Icon name="trash" size={18} /></TooltipButton><span>删除</span></div>
            <TooltipButton label="更多邮件操作" onClick={() => pushToast('更多邮件操作即将上线', 'info')}><Icon name="more" size={19} /></TooltipButton>
          </div>
          <div className="reading-scroll">
            <div className="reading-heading"><div><h1>{selectedMail.subject}</h1><div className="message-tags"><span className="tag tag-account"><ProviderMark provider={accounts.find((account) => account.id === selectedMail.accountId)?.provider ?? 'google'} size="sm" /> {accounts.find((account) => account.id === selectedMail.accountId)?.label ?? 'Google'}</span>{selectedMail.hasHtml && <span className="tag">HTML 邮件</span>}</div></div><TooltipButton label={selectedMail.starred ? '取消星标' : '添加星标'} className={`reading-star ${selectedMail.starred ? 'is-starred' : ''}`} onClick={() => toggleStar(selectedMail)}><Icon name="star" size={24} weight={selectedMail.starred ? 'Filled' : 'Outline'} /></TooltipButton></div>
            <div className="sender-row"><Avatar message={selectedMail} size="lg" /><div className="sender-copy"><div><strong>{selectedMail.senderName}</strong> <span>&lt;{selectedMail.from}&gt;</span></div><div className="recipient">收件人： Olivia Chen &lt;olivia.chen@gmail.com&gt;</div></div><time>{selectedMail.timestamp}<br /><span>今天</span></time><TooltipButton label="发件人更多信息"><Icon name="more" size={19} /></TooltipButton></div>
            <div className="message-content">
              {selectedMail.hasHtml && <div className="content-mode-row"><span>此邮件包含富文本内容</span><button type="button" className="text-action" onClick={() => setHtmlMode((value) => !value)}>{isHtmlMode ? '查看纯文本' : '渲染 HTML'} <Icon name="grid" size={14} /></button></div>}
              {isHtmlMode && selectedMail.hasHtml ? <div className="html-rendered" dangerouslySetInnerHTML={{ __html: sanitizeHtml(selectedMail.htmlBody ?? initialHtml) }} /> : selectedMail.body.map((paragraph) => <p key={paragraph}>{paragraph}</p>)}
            </div>
            {selectedMail.attachments && <div className="attachments"><div className="attachments-heading"><span><Icon name="paperclip" size={20} /> {selectedMail.attachments.length} 个附件</span><div><button type="button" onClick={() => { void Promise.all(selectedMail.attachments?.map(downloadAttachment) ?? []) }}><Icon name="download" size={17} /> 全部下载</button><button type="button" onClick={() => pushToast('正在保存到本地缓存', 'success')}><Icon name="cloud" size={17} /> 保存到云盘</button></div></div><div className="attachment-grid">{selectedMail.attachments.map((attachment) => <button type="button" className="attachment-card" key={attachment.id} onClick={() => { void downloadAttachment(attachment) }}><span className={`file-glyph file-${attachment.kind}`}>{attachment.kind === 'pdf' ? 'PDF' : attachment.kind === 'sheet' ? 'X' : 'FILE'}</span><span className="attachment-copy"><strong>{attachment.name}</strong><small>{attachment.size}</small></span><Icon name="download" size={17} /></button>)}</div></div>}
            <div className="reply-composer"><Avatar message={{ ...selectedMail, avatar: 'OC', accent: '#2a5596' }} size="sm" /><div className="reply-input" onClick={() => setComposeOpen(true)}>点击回复，或按 R 快速回复<div className="reply-tools"><span><Icon name="paperclip" size={19} /></span><span><Icon name="image" size={19} /></span><span className="reply-emoji">☺</span><span className="reply-a">A</span><button type="button" onClick={(event) => { event.stopPropagation(); setComposeOpen(true) }}>回复 <span>⌄</span></button></div></div></div>
          </div>
        </section>

        <AnimatePresence initial={false}>
          {isAuthPanelOpen && <motion.aside className="auth-panel" initial={prefersReducedMotion ? false : { x: 24, opacity: 0 }} animate={{ x: 0, opacity: 1 }} exit={{ x: 24, opacity: 0 }} transition={{ duration: 0.24 }}>
            <div className="auth-panel-header"><div><Icon name="key" size={20} /><strong>授权码助手</strong></div><TooltipButton label="关闭授权码助手" onClick={() => setAuthPanelOpen(false)}><Icon name="close" size={18} /></TooltipButton></div>
            <div className="auth-tabs"><button type="button" className="is-active"><Icon name="lock" size={16} />授权码</button><button type="button" onClick={() => setAccountModalOpen(true)}><Icon name="settings" size={16} />设置</button></div>
            <div className="auth-card"><div className="auth-illustration"><Icon name="shieldCheck" size={40} /></div><h2>快速获取授权码</h2><p>用于第三方服务登录验证</p><button className="gradient-button" type="button" onClick={() => setAccountModalOpen(true)}><Icon name="copy" size={17} />管理授权码</button><div className="auth-validity"><Icon name="clock" size={16} />授权码仅保存在本机安全存储</div></div>
            <div className="auth-panel-section"><div className="panel-section-title">账户</div>{accounts.map((account) => <button type="button" className="auth-account-row" key={account.id} onClick={() => { changeProvider(account.provider); setAccountEmail(account.email); setCustomImapHost(account.imapHost ?? 'imap.example.com'); setCustomImapPort(String(account.imapPort ?? 993)); setCustomImapSecurity(account.imapSecurity ?? 'tls'); setCustomSmtpHost(account.smtpHost ?? 'smtp.example.com'); setCustomSmtpPort(String(account.smtpPort ?? 465)); setCustomSmtpSecurity(account.smtpSecurity ?? 'tls'); setCustomAuthentication(account.authentication ?? (account.provider === 'outlook' ? 'oauth2' : 'app-password')); setAccountModalOpen(true) }}><ProviderMark provider={account.provider} size="sm" /><span><strong>{account.label}</strong><small>{account.email}</small></span><span className="auth-chevron">›</span></button>)}</div>
            <div className="auth-note"><Icon name="info" size={18} /><span>授权码仅用于登录验证<br />不会存储或同步到云端</span></div>
            <div className="auth-panel-foot"><button type="button" onClick={handleOpenProvider}><Icon name="link" size={15} />打开 {selectedProvider.label} 设置</button></div>
          </motion.aside>}
        </AnimatePresence>
        {!isAuthPanelOpen && <button className="auth-panel-reopen" type="button" onClick={() => setAuthPanelOpen(true)}><Icon name="key" size={18} />授权码助手</button>}
      </div>

      <AnimatePresence>
        {isSettingsOpen && <motion.div className="settings-popover" initial={{ opacity: 0, y: 8 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0, y: 8 }}><div className="settings-title"><span><Icon name="settings" size={17} />偏好设置</span><TooltipButton label="关闭设置" onClick={() => setSettingsOpen(false)}><Icon name="close" size={17} /></TooltipButton></div><div className="settings-row"><span><Icon name={theme === 'dark' ? 'moon' : 'theme'} size={17} /><span>外观主题<small>{theme === 'dark' ? '深色 · 午夜蓝' : '浅色 · 雪白'}</small></span></span><button type="button" className="theme-switch" onClick={() => setTheme((value) => value === 'dark' ? 'light' : 'dark')}><span className={theme === 'light' ? 'is-light' : ''}>{theme === 'dark' ? '深' : '浅'}</span></button></div><label className="settings-row css-row"><span><Icon name="brush" size={17} /><span>用户 CSS<small>可覆盖 MailGo 视觉变量</small></span></span><textarea value={customCss} onChange={(event) => setCustomCss(event.target.value)} placeholder="例如：:root { --accent: #ff6b8a; }" /></label><div className="settings-row"><span><Icon name="cloud" size={17} /><span>关闭时后台运行<small>最小化到系统托盘并继续同步</small></span></span><button type="button" className={`toggle-switch ${minimizeToTray ? 'is-on' : ''}`} onClick={() => { const next = !minimizeToTray; setMinimizeToTray(next); void invoke('app.set_minimize_to_tray', { enabled: next }).catch(() => undefined) }}><span /></button></div><div className="settings-actions"><button type="button" onClick={exportAccounts}><Icon name="download" size={16} />导出账户配置</button><button type="button" onClick={() => importInputRef.current?.click()} disabled={isImporting}><Icon name="folder" size={16} />{isImporting ? '导入中…' : '导入账户配置'}</button></div><input ref={importInputRef} type="file" accept="application/json,.json" hidden onChange={importAccounts} /></motion.div>}
      </AnimatePresence>

      <AnimatePresence>{isComposeOpen && <ComposeModal accountId={selectedAccountId ?? accounts[0]?.id} onClose={() => setComposeOpen(false)} onSent={() => { setComposeOpen(false); pushToast('邮件已发送', 'success') }} onError={(message) => pushToast(message, 'error')} />}</AnimatePresence>
      <AnimatePresence>{isAccountModalOpen && <AccountModal provider={provider} setProvider={changeProvider} providerDefinition={selectedProvider} accountEmail={accountEmail} setAccountEmail={setAccountEmail} authorizationCode={authorizationCode} setAuthorizationCode={setAuthorizationCode} showAuthorizationCode={showAuthorizationCode} setShowAuthorizationCode={setShowAuthorizationCode} customImapHost={customImapHost} setCustomImapHost={setCustomImapHost} customImapPort={customImapPort} setCustomImapPort={setCustomImapPort} customImapSecurity={customImapSecurity} setCustomImapSecurity={setCustomImapSecurity} customSmtpHost={customSmtpHost} setCustomSmtpHost={setCustomSmtpHost} customSmtpPort={customSmtpPort} setCustomSmtpPort={setCustomSmtpPort} customSmtpSecurity={customSmtpSecurity} setCustomSmtpSecurity={setCustomSmtpSecurity} customAuthentication={customAuthentication} setCustomAuthentication={setCustomAuthentication} onClose={() => setAccountModalOpen(false)} onOpenProvider={() => { void handleOpenProvider() }} onCopy={handleCopy} onAdd={handleAddAccount} />}</AnimatePresence>
      <div className="toast-stack" aria-live="polite">{toasts.map((toast) => <motion.div key={toast.id} className={`toast toast-${toast.tone}`} initial={{ opacity: 0, y: 12, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 12 }}><Icon name={toast.tone === 'success' ? 'checkCircle' : toast.tone === 'error' ? 'info' : 'bell'} size={17} /><span>{toast.message}</span></motion.div>)}</div>
    </div>
  )
}

function AccountModal({ provider, setProvider, providerDefinition, accountEmail, setAccountEmail, authorizationCode, setAuthorizationCode, showAuthorizationCode, setShowAuthorizationCode, customImapHost, setCustomImapHost, customImapPort, setCustomImapPort, customImapSecurity, setCustomImapSecurity, customSmtpHost, setCustomSmtpHost, customSmtpPort, setCustomSmtpPort, customSmtpSecurity, setCustomSmtpSecurity, customAuthentication, setCustomAuthentication, onClose, onOpenProvider, onCopy, onAdd }: { provider: Provider; setProvider: (provider: Provider) => void; providerDefinition: ReturnType<typeof providerFor>; accountEmail: string; setAccountEmail: (value: string) => void; authorizationCode: string; setAuthorizationCode: (value: string) => void; showAuthorizationCode: boolean; setShowAuthorizationCode: (value: boolean) => void; customImapHost: string; setCustomImapHost: (value: string) => void; customImapPort: string; setCustomImapPort: (value: string) => void; customImapSecurity: string; setCustomImapSecurity: (value: string) => void; customSmtpHost: string; setCustomSmtpHost: (value: string) => void; customSmtpPort: string; setCustomSmtpPort: (value: string) => void; customSmtpSecurity: string; setCustomSmtpSecurity: (value: string) => void; customAuthentication: string; setCustomAuthentication: (value: string) => void; onClose: () => void; onOpenProvider: () => void; onCopy: () => void; onAdd: () => void }) {
  return <motion.div className="modal-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}><motion.div className="account-modal" initial={{ opacity: 0, y: 14, scale: 0.98 }} animate={{ opacity: 1, y: 0, scale: 1 }} exit={{ opacity: 0, y: 8, scale: 0.98 }} role="dialog" aria-modal="true" aria-labelledby="add-account-title"><div className="modal-header"><div><Icon name="user" size={21} /><h2 id="add-account-title">添加账户</h2></div><TooltipButton label="关闭" onClick={onClose}><Icon name="close" size={19} /></TooltipButton></div><div className="account-modal-body"><div className="provider-chooser">{providerDefinitions.map((item) => <button key={item.id} type="button" className={`provider-option ${provider === item.id ? 'is-selected' : ''}`} onClick={() => setProvider(item.id)}><ProviderMark provider={item.id} size="md" /><span><strong>{item.label}</strong><small>{item.description}</small></span>{provider === item.id && <Icon name="checkCircle" size={19} />}</button>)}</div><div className="account-form"><label>邮箱地址<input type="email" value={accountEmail} onChange={(event) => setAccountEmail(event.target.value)} placeholder={provider === 'qq' ? 'yourname@qq.com' : 'name@example.com'} autoFocus /></label><label><span className="label-with-action">授权码<button type="button" onClick={onOpenProvider}>如何获取授权码？ <Icon name="link" size={13} /></button></span><span className="secret-input"><input type={showAuthorizationCode ? 'text' : 'password'} value={authorizationCode} onChange={(event) => setAuthorizationCode(event.target.value)} placeholder="粘贴邮箱授权码" /><button type="button" onClick={() => setShowAuthorizationCode(!showAuthorizationCode)} aria-label={showAuthorizationCode ? '隐藏授权码' : '显示授权码'}><Icon name={showAuthorizationCode ? 'eyeSlash' : 'eye'} size={17} /></button><button type="button" onClick={onCopy} aria-label="复制授权码"><Icon name="copy" size={17} /></button></span></label>{provider === 'other' && <div className="custom-transport-fields"><div className="transport-heading"><Icon name="settings" size={15} />自定义服务器</div><div className="transport-row"><label>IMAP 主机<input value={customImapHost} onChange={(event) => setCustomImapHost(event.target.value)} placeholder="imap.example.com" /></label><label>端口<input type="number" min="1" max="65535" value={customImapPort} onChange={(event) => setCustomImapPort(event.target.value)} /></label><label>安全<input value={customImapSecurity} onChange={(event) => setCustomImapSecurity(event.target.value)} placeholder="tls / starttls" /></label></div><div className="transport-row"><label>SMTP 主机<input value={customSmtpHost} onChange={(event) => setCustomSmtpHost(event.target.value)} placeholder="smtp.example.com" /></label><label>端口<input type="number" min="1" max="65535" value={customSmtpPort} onChange={(event) => setCustomSmtpPort(event.target.value)} /></label><label>安全<input value={customSmtpSecurity} onChange={(event) => setCustomSmtpSecurity(event.target.value)} placeholder="tls / starttls" /></label></div><label>认证方式<select value={customAuthentication} onChange={(event) => setCustomAuthentication(event.target.value)}><option value="password">密码 / 授权码</option><option value="app-password">应用专用密码</option><option value="oauth2">OAuth2 Bearer Token</option></select></label></div>}<div className="guide-box"><div className="guide-heading"><span><Icon name="key" size={17} />如何获取授权码？</span><em>{providerDefinition.label}</em></div>{providerDefinition.guide.map((step, index) => <div className="guide-step" key={step}><span className="step-number">{index + 1}</span><span><strong>{step}</strong><small>{index === 0 ? `登录 ${providerDefinition.label}，打开设置页面` : index === 1 ? '找到第三方客户端或账户安全选项' : '复制生成的授权凭据，返回此处粘贴'}</small></span>{index === 0 && <button type="button" onClick={onOpenProvider}>前往设置 <Icon name="link" size={13} /></button>}</div>)}</div></div></div><div className="modal-footer"><span><Icon name="shieldCheck" size={17} />凭据只保存在本机，不会上传到第三方</span><div><button className="secondary-button" type="button" onClick={onClose}>取消</button><button className="gradient-button" type="button" onClick={onAdd}><Icon name="rotate" size={17} />开始同步</button></div></div></motion.div></motion.div>
}

function ComposeModal({ accountId, onClose, onSent, onError }: { accountId?: string; onClose: () => void; onSent: () => void; onError: (message: string) => void }) {
  const [to, setTo] = useState('')
  const [subject, setSubject] = useState('')
  const [body, setBody] = useState('')
  const [isSending, setSending] = useState(false)
  const send = async () => {
    if (!to.includes('@')) {
      onError('请输入有效的收件人地址')
      return
    }
    setSending(true)
    try {
      if (window.ipc?.postMessage) {
        if (!accountId) throw new Error('请先添加一个可发送邮件的账户')
        await invoke('mail.send', { accountId, to: to.trim(), subject: subject.trim() || '(无主题)', textBody: body })
      } else {
        await new Promise((resolve) => window.setTimeout(resolve, 700))
      }
      onSent()
    } catch (error) {
      onError(error instanceof Error ? error.message : '邮件发送失败，请稍后重试')
    } finally {
      setSending(false)
    }
  }
  return <motion.div className="modal-backdrop compose-backdrop" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} onMouseDown={(event) => { if (event.target === event.currentTarget) onClose() }}><motion.div className="compose-modal" initial={{ opacity: 0, y: 20 }} animate={{ opacity: 1, y: 0 }} exit={{ opacity: 0, y: 20 }}><div className="compose-header"><strong>新邮件</strong><div><TooltipButton label="最小化撰写窗口"><span className="window-minimize" /></TooltipButton><TooltipButton label="关闭撰写窗口" onClick={onClose}><Icon name="close" size={17} /></TooltipButton></div></div><label>收件人<input autoFocus value={to} onChange={(event) => setTo(event.target.value)} placeholder="name@example.com" /></label><label>主题<input value={subject} onChange={(event) => setSubject(event.target.value)} placeholder="主题" /></label><textarea className="compose-body" value={body} onChange={(event) => setBody(event.target.value)} placeholder="写下你的邮件…" /><div className="compose-footer"><div><TooltipButton label="添加附件"><Icon name="paperclip" size={19} /></TooltipButton><TooltipButton label="插入图片"><Icon name="image" size={19} /></TooltipButton><TooltipButton label="格式"><span className="reply-a">A</span></TooltipButton></div><button type="button" className="gradient-button" onClick={send} disabled={isSending}>{isSending ? '发送中…' : '发送'}<Icon name="send" size={17} /></button></div></motion.div></motion.div>
}

export default App
