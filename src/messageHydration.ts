import type { MailMessage } from './types'

export const BODY_READ_AHEAD_RADIUS = 2
export const BODY_READ_AHEAD_LIMIT = 4

const BODY_LOADING_PLACEHOLDER = '正在加载邮件正文…'

export function mailNeedsBodyHydration(mail: MailMessage) {
  return mail.nativeUid != null
    && !mail.outboxId
    && mail.body.length === 1
    && mail.body[0] === BODY_LOADING_PLACEHOLDER
}

export function selectBodyHydrationCandidates(
  mails: MailMessage[],
  selectedMailId: string,
  limit = BODY_READ_AHEAD_LIMIT,
) {
  const selectedIndex = mails.findIndex((mail) => mail.id === selectedMailId)
  const boundedLimit = Number.isFinite(limit)
    ? Math.min(BODY_READ_AHEAD_LIMIT, Math.max(0, Math.trunc(limit)))
    : 0
  if (selectedIndex < 0 || boundedLimit === 0) return []

  const indices = [selectedIndex]
  for (let distance = 1; distance <= BODY_READ_AHEAD_RADIUS; distance += 1) {
    indices.push(selectedIndex + distance, selectedIndex - distance)
  }

  const identities = new Set<string>()
  const candidates: MailMessage[] = []
  for (const index of indices) {
    const mail = mails[index]
    if (!mail || !mailNeedsBodyHydration(mail)) continue
    const identity = `${mail.accountId}\u0000${(mail.nativeFolder ?? 'INBOX').toLocaleLowerCase()}\u0000${mail.nativeUid}`
    if (identities.has(identity)) continue
    identities.add(identity)
    candidates.push(mail)
    if (candidates.length === boundedLimit) break
  }
  return candidates
}
