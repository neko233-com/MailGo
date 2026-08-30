export type Provider = 'google' | 'qq' | 'outlook' | 'other'

export type ThemeMode = 'dark' | 'light'

export type FolderId = 'inbox' | 'starred' | 'sent' | 'drafts' | 'archive' | 'spam' | 'trash'

export type SmartCategory = 'apple-connect' | 'apple-ads' | 'social' | 'finance'

export interface MailAttachment {
  id: string
  name: string
  size: string
  kind: 'pdf' | 'sheet' | 'image' | 'file'
}

export interface MailMessage {
  id: string
  accountId: string
  folder: FolderId
  category?: SmartCategory
  from: string
  senderName: string
  subject: string
  preview: string
  timestamp: string
  dateGroup: string
  unread: boolean
  starred: boolean
  accent: string
  avatar: string
  body: string[]
  attachments?: MailAttachment[]
  hasHtml?: boolean
}

export interface MailAccount {
  id: string
  provider: Provider
  label: string
  email: string
  unread: number
  accent: string
  status: 'synced' | 'syncing' | 'offline' | 'needs-auth'
  lastSync: string
}

export interface ProviderDefinition {
  id: Provider
  label: string
  description: string
  accent: string
  icon: string
  authUrl: string
  guide: string[]
  requiresAuthCode: boolean
}

export interface ExportedAccount {
  id: string
  provider: Provider
  label: string
  email: string
  status: 'requires-reauth'
  secretRef: string
}

export interface NativeState {
  accounts: MailAccount[]
  theme: ThemeMode
  minimizeToTray: boolean
  offlineMode: boolean
}
