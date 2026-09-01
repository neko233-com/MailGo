export type Provider = 'google' | 'qq' | 'outlook' | 'other'

export type ThemeMode = 'dark' | 'light'

export type FolderId = 'inbox' | 'starred' | 'sent' | 'drafts' | 'archive' | 'spam' | 'trash'

export type SmartCategory = 'apple-connect' | 'apple-ads' | 'social' | 'ads' | 'finance'

export interface MailAttachment {
  id: string
  name: string
  size: string
  kind: 'pdf' | 'sheet' | 'image' | 'file'
  nativeIndex?: number
}

export interface MailMessage {
  id: string
  accountId: string
  folder: FolderId
  category?: SmartCategory
  from: string
  senderName: string
  subject: string
  to?: string[]
  cc?: string[]
  preview: string
  timestamp: string
  dateGroup: string
  unread: boolean
  starred: boolean
  isAd?: boolean
  accent: string
  avatar: string
  body: string[]
  attachments?: MailAttachment[]
  hasHtml?: boolean
  htmlBody?: string
  nativeUid?: number
  nativeFolder?: string
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
  imapHost?: string
  imapPort?: number
  imapSecurity?: string
  smtpHost?: string
  smtpPort?: number
  smtpSecurity?: string
  authentication?: string
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
  folders?: Record<string, string[]>
  folderLabels?: Record<string, Record<string, string>>
  theme: ThemeMode
  minimizeToTray: boolean
  offlineMode: boolean
  notificationsEnabled: boolean
  remoteImagesEnabled: boolean
  hideAds: boolean
}

export interface NativeSyncItem {
  accountId: string
  folder: string
  fetched: number
  unread: number
  cachePath: string
  syncedAt: string
  folders?: string[]
  folderLabels?: Record<string, string>
}

export interface NativeSyncResponse {
  accepted: boolean
  mode: string
  synced: NativeSyncItem[]
  failed: { accountId: string; message: string }[]
}

export interface NativeSearchResponse {
  messages: NativeCachedMessage[]
  truncated: boolean
  failed: { accountId: string; message: string }[]
}

export interface NativeQueueStatus {
  flags: number
  moves: number
  total: number
}

export interface NativeOutboxStatus {
  total: number
  pending: number
  paused: number
}

export interface NativeCacheStats {
  totalBytes: number
  fileCount: number
  mailBytes: number
  attachmentBytes: number
  draftBytes: number
  outboxBytes: number
  operationBytes: number
  otherBytes: number
  unreadableEntries: number
  truncated: boolean
  scannedAt: number
}

export interface NativeCacheStatsResponse {
  state: 'loading' | 'ready' | 'error'
  stats?: NativeCacheStats
  message?: string
}

export interface NativeDraft {
  id: string
  accountId: string
  to: string
  cc: string
  bcc: string
  subject: string
  body: string
  htmlMode: boolean
  updatedAt: number
}

export interface NativeCachedAttachment {
  index: number
  fileName: string
  contentType: string
  contentId?: string
  size: number
  cachePath?: string
}

export interface NativeAttachmentResponse {
  fileName: string
  contentType: string
  dataBase64: string
}

export interface NativeAttachmentStartResponse {
  downloadId: string
  fileName: string
  contentType: string
  size: number
  chunkSize: number
}

export interface NativeAttachmentChunkResponse {
  downloadId: string
  offset: number
  nextOffset: number
  done: boolean
  dataBase64: string
}

export interface NativeAttachmentUploadStartResponse {
  uploadId: string
  chunkSize: number
  size: number
  done: boolean
}

export interface NativeAttachmentUploadChunkResponse {
  uploadId: string
  offset: number
  nextOffset: number
  done: boolean
}

export interface NativeDeviceStartResponse {
  sessionId: string
  userCode: string
  verificationUri: string
  message?: string
  expiresIn: number
  interval: number
}

export interface NativeAuthStartResponse {
  sessionId: string
  authorizationUrl: string
  redirectUri: string
  state: string
  expiresIn: number
}

export interface NativeCachedMessage {
  id: string
  accountId: string
  folder: string
  uid: number
  subject: string
  senderName: string
  senderEmail: string
  to?: string[]
  cc?: string[]
  receivedAt?: string
  unread: boolean
  starred: boolean
  category: 'apple-connect' | 'apple-ads' | 'social' | 'ads' | 'inbox'
  isAd: boolean
  preview: string
  textBody: string
  htmlBody?: string
  attachments: NativeCachedAttachment[]
  rawPath?: string
}

export interface NativeMailbox {
  schemaVersion: number
  accountId: string
  folder: string
  uidValidity?: number
  syncedAt: string
  messages: NativeCachedMessage[]
  oldestUid?: number
  hasMore?: boolean
}

export interface NativeMailboxResponse {
  offline: boolean
  mailbox?: NativeMailbox
}

export interface NativeMessageResponse {
  offline: boolean
  message: NativeCachedMessage
}
