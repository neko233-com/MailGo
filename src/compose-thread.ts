import type { MailMessage } from './types'

export type ComposeMode = 'new' | 'reply' | 'reply-all' | 'forward'

export interface ComposeThreadHeaders {
  inReplyTo?: string
  references: string[]
}

const MAX_MESSAGE_ID_BYTES = 512
const MAX_THREAD_REFERENCES = 32

function normalizeMessageId(value: string | undefined) {
  if (!value) return undefined
  const normalized = value.trim().replace(/^<|>$/g, '')
  const separator = normalized.lastIndexOf('@')
  if (!normalized
    || normalized.length > MAX_MESSAGE_ID_BYTES
    || separator <= 0
    || separator === normalized.length - 1
    || Array.from(normalized).some((character) => {
      const code = character.charCodeAt(0)
      return code <= 0x20 || code >= 0x7f || character === '<' || character === '>'
    })) return undefined
  return normalized
}

function boundReferences(values: Array<string | undefined>) {
  const unique: string[] = []
  const seen = new Set<string>()
  for (const value of values) {
    const normalized = normalizeMessageId(value)
    if (!normalized || seen.has(normalized)) continue
    seen.add(normalized)
    unique.push(normalized)
  }
  if (unique.length <= MAX_THREAD_REFERENCES) return unique
  return [unique[0], ...unique.slice(-(MAX_THREAD_REFERENCES - 1))]
}

export function buildComposeThreadHeaders(mode: ComposeMode, source?: MailMessage): ComposeThreadHeaders {
  if (!source || (mode !== 'reply' && mode !== 'reply-all')) return { references: [] }
  const parentId = normalizeMessageId(source.messageId)
  if (!parentId) return { references: [] }
  const ancestors = source.references?.length ? source.references : [source.inReplyTo]
  return {
    inReplyTo: parentId,
    references: boundReferences([...ancestors, parentId]),
  }
}
