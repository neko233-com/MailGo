export type Provider = 'google' | 'qq' | 'outlook' | 'other'

export type ThemeMode = 'dark' | 'light'

export type FolderId = 'inbox' | 'starred' | 'snoozed' | 'outbox' | 'sent' | 'drafts' | 'archive' | 'spam' | 'trash'

export type NativeFolderRole = Extract<FolderId, 'inbox' | 'sent' | 'drafts' | 'archive' | 'spam' | 'trash'>

export type NativeFolderRoles = Partial<Record<NativeFolderRole, string>>

export type SmartCategory = 'apple-connect' | 'apple-ads' | 'social' | 'ads' | 'finance'

export type MailRuleKind = 'sender' | 'domain'

export interface NativeMailRule {
  id: string
  accountId?: string
  kind: MailRuleKind
  value: string
  createdAt: number
}

export interface NativeMailRuleSnapshot {
  rules: NativeMailRule[]
  removed?: boolean
  added?: NativeMailRule
}

export interface MailAttachment {
  id: string
  name: string
  size: string
  kind: 'pdf' | 'sheet' | 'image' | 'file'
  nativeIndex?: number
}

export interface MailMessage {
  id: string
  messageId?: string
  inReplyTo?: string
  references?: string[]
  threadId?: string
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
  receivedAt?: string
  dateGroup: string
  unread: boolean
  starred: boolean
  isAd?: boolean
  blocked?: boolean
  blockedRuleId?: string
  accent: string
  avatar: string
  body: string[]
  attachments?: MailAttachment[]
  hasHtml?: boolean
  htmlBody?: string
  nativeUid?: number
  nativeFolder?: string
  outboxId?: string
  outboxState?: NativeOutboxItemState
  outboxScheduledAt?: number
  snoozedUntil?: number
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
  signature: string
  folderRoles?: NativeFolderRoles
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
  signature?: string
}

export interface NativeState {
  accounts: MailAccount[]
  folders?: Record<string, string[]>
  folderLabels?: Record<string, Record<string, string>>
  folderRoles?: Record<string, NativeFolderRoles>
  theme: ThemeMode
  minimizeToTray: boolean
  offlineMode: boolean
  notificationsEnabled: boolean
  remoteImagesEnabled: boolean
  hideAds: boolean
  undoSendSeconds?: number
}

export interface NativeSendResponse {
  sent: boolean
  queued: boolean
  accountId: string
  outboxId?: string
  draftId?: string
  offline?: boolean
  undoable?: boolean
  undoSeconds?: number
  undoExpiresAt?: number
  scheduled?: boolean
  scheduledFor?: number
}

export interface NativeUndoSendResponse {
  accountId: string
  outboxId: string
  status: 'cancelled' | 'missing' | 'too-late'
}

export interface NativeSyncItem {
  accountId: string
  folder: string
  fetched: number
  unread: number
  newUnread: number
  cachePath: string
  syncedAt: string
  folders?: string[]
  folderLabels?: Record<string, string>
  folderRoles?: NativeFolderRoles
}

export interface NativeSyncResponse {
  accepted: boolean
  mode: string
  synced: NativeSyncItem[]
  failed: { accountId: string; message: string }[]
}

export type NativeConnectionDiagnosticStatus = 'ok' | 'authentication' | 'rate-limit' | 'network' | 'tls' | 'provider'

export interface NativeConnectionDiagnosticChannel {
  ok: boolean
  status: NativeConnectionDiagnosticStatus
  latencyMs: number
}

export interface NativeConnectionDiagnostic {
  accountId: string
  checkedAt: string
  ok: boolean
  incoming: NativeConnectionDiagnosticChannel
  outgoing: NativeConnectionDiagnosticChannel
}

export interface NativeSearchResponse {
  messages: NativeCachedMessage[]
  truncated: boolean
  failed: { accountId: string; message: string }[]
}

export interface NativeLocalSearchResponse {
  messages: NativeCachedMessage[]
  truncated: boolean
  indexing: boolean
}

export interface NativeRecipientSuggestion {
  name: string
  email: string
  frequency: number
  lastSeen?: string
}

export interface NativeRecipientSuggestionResponse {
  suggestions: NativeRecipientSuggestion[]
  truncated: boolean
  indexing: boolean
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
  scheduled?: number
  userScheduled?: number
  undoable?: number
}

export type NativeOutboxItemState = 'scheduled' | 'pending' | 'retrying' | 'paused'

export interface NativeOutboxAttachmentSummary {
  fileName: string
  contentType: string
  size: number
  inline: boolean
}

export interface NativeOutboxItem {
  id: string
  accountId: string
  draftId?: string
  to: string
  cc: string
  bcc: string
  subject: string
  preview: string
  createdAt: number
  updatedAt: number
  nextAttemptAt: number
  scheduledAt?: number
  attempts: number
  state: NativeOutboxItemState
  lastError?: string
  attachments: NativeOutboxAttachmentSummary[]
}

export interface NativeOutboxSnapshot {
  status: NativeOutboxStatus
  items: NativeOutboxItem[]
}

export interface NativeOutboxRecallResponse {
  status: 'recalled' | 'missing' | 'too-late'
  draft?: NativeDraft
}

export interface NativeOutboxActionResponse {
  accountId: string
  outboxId: string
  status: 'retried' | 'discarded' | 'missing' | 'too-late'
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
  htmlBody?: string
  inReplyTo?: string
  references: string[]
  attachments: NativeDraftAttachment[]
  updatedAt: number
}

export interface NativeDraftAttachment {
  id: string
  fileName: string
  contentType: string
  contentId?: string
  size: number
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
  attachmentId?: string
  fileName: string
  contentType: string
  contentId?: string
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
  messageId?: string
  inReplyTo?: string
  references?: string[]
  threadId: string
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
  blocked: boolean
  blockedRuleId?: string
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
  unchanged?: boolean
  localHasMore?: boolean
  remoteHasMore?: boolean
  totalCached?: number
  revision?: number
}

export interface NativeMessageResponse {
  offline: boolean
  message: NativeCachedMessage
}

export interface NativeSnoozedItem {
  message: NativeCachedMessage
  createdAt: number
  wakeAt: number
}

export interface NativeSnoozeSnapshot {
  items: NativeSnoozedItem[]
  nextWakeAt?: number
  removed?: boolean
}
