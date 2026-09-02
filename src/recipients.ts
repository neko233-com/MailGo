import type { NativeRecipientSuggestion } from './types'

const unsafeRecipientCharacter = /[\u0000-\u001f\u007f\u200b-\u200f\u202a-\u202e\u2060-\u2069\ufeff]/u
const unsafeRecipientCharacters = /[\u0000-\u001f\u007f\u200b-\u200f\u202a-\u202e\u2060-\u2069\ufeff]/gu
const recipientDelimiter = /[,;\r\n]/u

function lastRecipientDelimiter(value: string) {
  let boundary = -1
  for (let index = 0; index < value.length; index += 1) {
    if (recipientDelimiter.test(value[index])) boundary = index
  }
  return boundary
}

export function activeRecipientQuery(value: string) {
  return value.slice(lastRecipientDelimiter(value) + 1).trim().slice(0, 256)
}

export function recipientEmails(value: string) {
  const emails = new Set<string>()
  for (const token of value.split(recipientDelimiter)) {
    const trimmed = token.trim()
    const bracketed = trimmed.match(/<([^<>]+)>\s*$/u)?.[1]?.trim()
    const email = (bracketed ?? trimmed).toLowerCase()
    if (email.includes('@') && !unsafeRecipientCharacter.test(email)) emails.add(email)
  }
  return emails
}

export function isSafeSuggestedEmail(value: string) {
  const email = value.trim()
  const separator = email.indexOf('@')
  return email.length > 3
    && email.length <= 320
    && separator > 0
    && separator === email.lastIndexOf('@')
    && separator < email.length - 3
    && email.slice(separator + 1).includes('.')
    && !/[\s,;<>]/u.test(email)
    && !unsafeRecipientCharacter.test(email)
}

export function formatRecipientSuggestion(suggestion: NativeRecipientSuggestion) {
  const email = suggestion.email.trim()
  if (!isSafeSuggestedEmail(email)) throw new Error('本机联系人地址无效')
  const name = suggestion.name
    .replace(unsafeRecipientCharacters, '')
    .replace(/[<>,;"\\]/gu, ' ')
    .replace(/\s+/gu, ' ')
    .trim()
    .slice(0, 80)
  return name && name.toLowerCase() !== email.toLowerCase() ? `${name} <${email}>` : email
}

export function applyRecipientSuggestion(value: string, suggestion: NativeRecipientSuggestion) {
  const boundary = lastRecipientDelimiter(value)
  const prefix = value.slice(0, boundary + 1)
  const spacing = prefix && !/\s$/u.test(prefix) ? ' ' : ''
  return `${prefix}${spacing}${formatRecipientSuggestion(suggestion)}, `
}

export function filterRecipientDirectory(
  directory: NativeRecipientSuggestion[],
  query: string,
  excluded: Set<string>,
  limit = 8,
) {
  const words = query.toLowerCase().split(/[^\p{L}\p{N}@._+-]+/u).filter(Boolean)
  if (words.length === 0) return []
  return directory
    .filter((suggestion) => {
      if (!isSafeSuggestedEmail(suggestion.email) || excluded.has(suggestion.email.toLowerCase())) return false
      const haystack = `${suggestion.name} ${suggestion.email}`.toLowerCase()
      return words.every((word) => haystack.includes(word))
    })
    .sort((left, right) => right.frequency - left.frequency || right.email.localeCompare(left.email))
    .slice(0, Math.max(1, Math.min(20, limit)))
}
