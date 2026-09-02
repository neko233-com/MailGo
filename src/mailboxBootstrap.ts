const INITIAL_MAILBOX_POLL_DELAY_MS = 120
const MAX_MAILBOX_POLL_DELAY_MS = 1_000
const MAILBOX_POLL_BACKOFF = 1.6

type MailboxBootstrapOptions<T> = {
  isSyncSettled: () => boolean
  readMailbox: () => Promise<T>
  hasMailbox: (result: T) => boolean
  revealMailbox: (result: T) => void
  sleep?: (delayMs: number) => Promise<void>
}

type SelectedMailboxView = {
  accountId: string | null
  folder: string
  nativeFolder: unknown
}

export function shouldSelectFirstRevealedMessage(view: SelectedMailboxView, accountId: string) {
  return view.accountId === accountId && view.folder === 'inbox' && !view.nativeFolder
}

function defaultSleep(delayMs: number) {
  return new Promise<void>((resolve) => window.setTimeout(resolve, delayMs))
}

/**
 * Poll the local cache while a longer provider sync is still running. The INBOX is persisted
 * before secondary folders, so revealing that first committed page removes a renderer-side
 * waterfall without opening a second IMAP session or waiting for Sent/Drafts/Spam/Trash.
 */
export async function revealFirstMailboxWhileSyncing<T>({
  isSyncSettled,
  readMailbox,
  hasMailbox,
  revealMailbox,
  sleep = defaultSleep,
}: MailboxBootstrapOptions<T>): Promise<boolean> {
  let delayMs = 0
  while (!isSyncSettled()) {
    if (delayMs > 0) await sleep(delayMs)
    if (isSyncSettled()) break

    let result: T
    try {
      result = await readMailbox()
    } catch {
      // A cache transaction may be committing between polls; the next bounded read can retry.
      delayMs = delayMs === 0
        ? INITIAL_MAILBOX_POLL_DELAY_MS
        : Math.min(MAX_MAILBOX_POLL_DELAY_MS, Math.ceil(delayMs * MAILBOX_POLL_BACKOFF))
      continue
    }
    if (hasMailbox(result)) {
      revealMailbox(result)
      return true
    }
    delayMs = delayMs === 0
      ? INITIAL_MAILBOX_POLL_DELAY_MS
      : Math.min(MAX_MAILBOX_POLL_DELAY_MS, Math.ceil(delayMs * MAILBOX_POLL_BACKOFF))
  }
  return false
}
