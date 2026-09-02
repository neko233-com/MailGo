import type { MailMessage, MailRuleKind, NativeMailRule } from './types'

const MAX_RULE_VALUE_LENGTH = 320

export function normalizeRuleDomain(value: string) {
  const normalized = value.trim().replace(/^@+/, '').replace(/\.+$/, '').toLowerCase()
  if (!normalized || normalized.length > 253 || /\s/.test(normalized) || !/^[\x00-\x7F]+$/.test(normalized)) {
    throw new Error('请输入有效的发件域名')
  }
  const labels = normalized.split('.')
  if (labels.some((label) => !label || label.length > 63 || label.startsWith('-') || label.endsWith('-') || !/^[a-z0-9-]+$/.test(label))) {
    throw new Error('请输入有效的发件域名')
  }
  return normalized
}

export function normalizeRuleSender(value: string) {
  const normalized = value.trim().toLowerCase()
  if (!normalized || normalized.length > MAX_RULE_VALUE_LENGTH || /[\s<>\u0000-\u001f\u007f]/.test(normalized)) {
    throw new Error('请输入完整、有效的发件人邮箱')
  }
  const separator = normalized.lastIndexOf('@')
  const local = normalized.slice(0, separator)
  const domain = normalized.slice(separator + 1)
  if (separator <= 0 || local.length > 64 || local.includes('@')) {
    throw new Error('请输入完整、有效的发件人邮箱')
  }
  return `${local}@${normalizeRuleDomain(domain)}`
}

export function normalizeRuleValue(kind: MailRuleKind, value: string) {
  return kind === 'sender' ? normalizeRuleSender(value) : normalizeRuleDomain(value)
}

export function domainFromSender(sender: string) {
  try {
    const normalized = normalizeRuleSender(sender)
    return normalized.slice(normalized.lastIndexOf('@') + 1)
  } catch {
    return ''
  }
}

export function mailMatchesRule(mail: MailMessage, rule: NativeMailRule) {
  if (rule.accountId && rule.accountId.toLowerCase() !== mail.accountId.toLowerCase()) return false
  if (rule.kind === 'sender') {
    try {
      return normalizeRuleSender(mail.from) === rule.value
    } catch {
      return false
    }
  }
  const domain = domainFromSender(mail.from)
  return domain === rule.value || domain.endsWith(`.${rule.value}`)
}

export function applyMailRules(mail: MailMessage, rules: NativeMailRule[]): MailMessage {
  const matched = rules.find((rule) => mailMatchesRule(mail, rule))
  return {
    ...mail,
    blocked: Boolean(matched),
    blockedRuleId: matched?.id,
  }
}
