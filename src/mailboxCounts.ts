import type { FolderId, MailAccount, MailMessage } from './types'

const FIXED_UNREAD_FOLDERS = ['inbox', 'sent', 'archive', 'spam', 'trash'] as const satisfies readonly FolderId[]
const FIXED_UNREAD_FOLDER_SET = new Set<FolderId>(FIXED_UNREAD_FOLDERS)

export function nativeFolderName(account: MailAccount, folder: FolderId): string {
  if (folder === 'outbox') return '__MAILGO_LOCAL_OUTBOX__'
  if (folder === 'snoozed') return '__MAILGO_LOCAL_SNOOZED__'
  if (folder === 'inbox') return 'INBOX'
  if (folder === 'sent') return account.provider === 'google' ? '[Gmail]/Sent Mail' : account.provider === 'outlook' ? 'Sent Items' : 'Sent Messages'
  if (folder === 'drafts') return account.provider === 'google' ? '[Gmail]/Drafts' : 'Drafts'
  if (folder === 'spam') return account.provider === 'google' ? '[Gmail]/Spam' : account.provider === 'outlook' ? 'Junk Email' : 'Spam'
  if (folder === 'trash') return account.provider === 'google' ? '[Gmail]/Trash' : account.provider === 'outlook' ? 'Deleted Items' : 'Trash'
  return account.provider === 'google' ? '[Gmail]/All Mail' : 'Archive'
}

export function isSameNativeFolder(left: string, right: string) {
  return left.toLocaleLowerCase() === right.toLocaleLowerCase()
}

export interface MailboxCountIndex {
  fixedUnread: Map<FolderId, number>
  nativeUnread: Map<string, number>
  hiddenUnreadByAccount: Map<string, number>
}

export function nativeFolderCountKey(accountId: string, folder: string) {
  return `${accountId.length}:${accountId}${folder.toLocaleLowerCase()}`
}

export function buildMailboxCountIndex(mails: readonly MailMessage[], accounts: readonly MailAccount[]): MailboxCountIndex {
  const fixedUnread = new Map<FolderId, number>()
  const nativeUnread = new Map<string, number>()
  const hiddenUnreadByAccount = new Map<string, number>()
  const nativeFoldersByAccount = new Map<string, Map<string, FolderId>>()
  for (const account of accounts) {
    const nativeFolders = new Map<string, FolderId>()
    for (const folder of FIXED_UNREAD_FOLDERS) {
      nativeFolders.set(nativeFolderName(account, folder).toLocaleLowerCase(), folder)
    }
    nativeFoldersByAccount.set(account.id, nativeFolders)
  }

  for (const mail of mails) {
    if (!mail.unread) continue
    if (mail.blocked || mail.snoozedUntil != null) {
      hiddenUnreadByAccount.set(mail.accountId, (hiddenUnreadByAccount.get(mail.accountId) ?? 0) + 1)
      continue
    }
    if (mail.nativeFolder) {
      const key = nativeFolderCountKey(mail.accountId, mail.nativeFolder)
      nativeUnread.set(key, (nativeUnread.get(key) ?? 0) + 1)
    }
    if (mail.starred) fixedUnread.set('starred', (fixedUnread.get('starred') ?? 0) + 1)

    const folder = mail.nativeFolder
      ? nativeFoldersByAccount.get(mail.accountId)?.get(mail.nativeFolder.toLocaleLowerCase())
      : mail.folder
    if (!folder || !FIXED_UNREAD_FOLDER_SET.has(folder)) continue
    fixedUnread.set(folder, (fixedUnread.get(folder) ?? 0) + 1)
  }
  return { fixedUnread, nativeUnread, hiddenUnreadByAccount }
}
