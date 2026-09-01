import type { MailMessage } from './types'

export interface MailThread {
  key: string
  messages: MailMessage[]
  latest: MailMessage
  participants: string[]
  unreadCount: number
}

type IndexedMail = { mail: MailMessage; index: number }

function threadScope(mail: MailMessage) {
  const folder = mail.nativeFolder?.trim() || mail.folder
  return `${mail.accountId}\u0000${folder}`
}

function boundedAnchor(value: string | undefined) {
  const anchor = value?.trim()
  return anchor && anchor.length <= 512 && !/[\r\n\0]/.test(anchor) ? anchor : undefined
}

function compareNewestFirst(left: IndexedMail, right: IndexedMail) {
  const leftTime = left.mail.receivedAt ? Date.parse(left.mail.receivedAt) : Number.NaN
  const rightTime = right.mail.receivedAt ? Date.parse(right.mail.receivedAt) : Number.NaN
  if (Number.isFinite(leftTime) && Number.isFinite(rightTime) && leftTime !== rightTime) return rightTime - leftTime
  return left.index - right.index
}

export function buildMailThreads(mails: MailMessage[]): MailThread[] {
  const parents = new Map<string, string>()
  const nodes: string[] = []
  const find = (node: string) => {
    let root = node
    while (parents.get(root) !== root) root = parents.get(root) ?? root
    let cursor = node
    while (parents.get(cursor) !== root) {
      const next = parents.get(cursor) ?? root
      parents.set(cursor, root)
      cursor = next
    }
    return root
  }
  const ensure = (node: string) => {
    if (!parents.has(node)) parents.set(node, node)
  }
  const union = (left: string, right: string) => {
    ensure(left)
    ensure(right)
    const leftRoot = find(left)
    const rightRoot = find(right)
    if (leftRoot !== rightRoot) parents.set(leftRoot, rightRoot)
  }

  mails.forEach((mail) => {
    const scope = threadScope(mail)
    const ownAnchor = boundedAnchor(mail.messageId) ?? `ui:${mail.id}`
    const ownNode = `${scope}\u0000${ownAnchor}`
    ensure(ownNode)
    nodes.push(ownNode)
    const anchors = [mail.threadId, mail.inReplyTo, ...(mail.references ?? [])]
    for (const candidate of anchors) {
      const anchor = boundedAnchor(candidate)
      if (anchor) union(ownNode, `${scope}\u0000${anchor}`)
    }
  })

  const grouped = new Map<string, IndexedMail[]>()
  mails.forEach((mail, index) => {
    const key = find(nodes[index])
    const messages = grouped.get(key)
    if (messages) messages.push({ mail, index })
    else grouped.set(key, [{ mail, index }])
  })

  const threads = [...grouped.entries()].map(([key, indexed]) => {
    const newestFirst = [...indexed].sort(compareNewestFirst)
    const participants: string[] = []
    for (const { mail } of newestFirst) {
      const participant = mail.senderName.trim() || mail.from.trim()
      if (participant && !participants.some((known) => known.toLocaleLowerCase() === participant.toLocaleLowerCase())) {
        participants.push(participant)
      }
      if (participants.length === 4) break
    }
    return {
      firstIndex: Math.min(...indexed.map(({ index }) => index)),
      thread: {
        key,
        messages: newestFirst.map(({ mail }) => mail).reverse(),
        latest: newestFirst[0].mail,
        participants,
        unreadCount: indexed.reduce((count, { mail }) => count + (mail.unread ? 1 : 0), 0),
      } satisfies MailThread,
    }
  })
  threads.sort((left, right) => compareNewestFirst(
    { mail: left.thread.latest, index: left.firstIndex },
    { mail: right.thread.latest, index: right.firstIndex },
  ))
  return threads.map(({ thread }) => thread)
}
