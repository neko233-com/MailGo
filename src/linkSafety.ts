export const MAX_EXTERNAL_URL_BYTES = 4 * 1024

export type ExternalLinkKind = 'https' | 'mailto'
export type ExternalLinkRisk = 'normal' | 'caution'

export type ExternalLinkInspection = {
  url: string
  kind: ExternalLinkKind
  primaryLabel: string
  secondaryLabel: string
  hasHiddenParameters: boolean
  warnings: string[]
  risk: ExternalLinkRisk
}

const utf8Encoder = new TextEncoder()
const unsafeUrlControl = /[\u0000-\u001f\u007f]/u
const unsafeDisplayFormat = /[\u200b-\u200f\u202a-\u202e\u2060-\u2069\ufeff]/u
const domainLikeText = /^(?:https:\/\/)?(?:www\.)?(?:[a-z\d](?:[a-z\d-]{0,61}[a-z\d])?\.)+[a-z]{2,63}(?::\d{1,5})?(?:[/?#][^\s]*)?$/iu

function isIpHostname(hostname: string) {
  if (hostname.startsWith('[') && hostname.endsWith(']')) return true
  const parts = hostname.split('.')
  return parts.length === 4 && parts.every((part) => /^\d{1,3}$/u.test(part) && Number(part) <= 255)
}

function displayHostname(value: string) {
  return value.toLowerCase().replace(/^www\./u, '')
}

function hostsAreRelated(left: string, right: string) {
  const a = displayHostname(left)
  const b = displayHostname(right)
  return a === b || a.endsWith(`.${b}`) || b.endsWith(`.${a}`)
}

function visibleHostname(text: string | undefined) {
  const candidate = text?.trim().replace(/^[<\[(]+|[>\])]+$/gu, '')
  if (!candidate || candidate.length > 512 || !domainLikeText.test(candidate)) return null
  try {
    return new URL(candidate.startsWith('https://') ? candidate : `https://${candidate}`).hostname
  } catch {
    return null
  }
}

function boundedPathname(pathname: string) {
  if (!pathname || pathname === '/') return '站点首页'
  const limit = 180
  return pathname.length > limit ? `${pathname.slice(0, limit - 1)}…` : pathname
}

function decodeMailRecipients(pathname: string) {
  try {
    return decodeURIComponent(pathname)
  } catch {
    return pathname
  }
}

export function inspectExternalLink(rawHref: string, visibleText?: string): ExternalLinkInspection {
  const href = rawHref.trim()
  if (!href || utf8Encoder.encode(href).byteLength > MAX_EXTERNAL_URL_BYTES || unsafeUrlControl.test(href)) {
    throw new Error('邮件链接为空、过长或包含不安全字符')
  }
  if (/%0d|%0a/iu.test(href)) {
    throw new Error('邮件链接包含换行注入')
  }

  let parsed: URL
  try {
    parsed = new URL(href)
  } catch {
    throw new Error('邮件链接格式无效')
  }

  if (parsed.protocol === 'https:') {
    if (!parsed.hostname || parsed.username || parsed.password) {
      throw new Error('HTTPS 链接缺少域名或包含嵌入凭据')
    }

    const warnings: string[] = []
    const shownHostname = visibleHostname(visibleText)
    if (shownHostname && !hostsAreRelated(shownHostname, parsed.hostname)) {
      warnings.push(`邮件显示文字指向 ${shownHostname}，实际将打开 ${parsed.hostname}`)
    }
    if (parsed.hostname.split('.').some((label) => label.startsWith('xn--'))) {
      warnings.push('目标使用国际化域名编码（Punycode），请仔细核对域名')
    }
    if (isIpHostname(parsed.hostname)) {
      warnings.push('目标是 IP 地址，而不是常规网站域名')
    }
    if (parsed.port && parsed.port !== '443') {
      warnings.push(`目标使用非标准 HTTPS 端口 ${parsed.port}`)
    }

    return {
      url: parsed.href,
      kind: 'https',
      primaryLabel: parsed.host,
      secondaryLabel: boundedPathname(parsed.pathname),
      hasHiddenParameters: Boolean(parsed.search || parsed.hash),
      warnings,
      risk: warnings.length > 0 ? 'caution' : 'normal',
    }
  }

  if (parsed.protocol === 'mailto:') {
    if (parsed.username || parsed.password || parsed.host || !parsed.pathname) {
      throw new Error('邮件地址链接格式无效')
    }
    const recipients = decodeMailRecipients(parsed.pathname).trim()
    if (!recipients || unsafeUrlControl.test(recipients) || unsafeDisplayFormat.test(recipients)) {
      throw new Error('邮件地址链接缺少有效收件人')
    }
    const warnings = recipients.includes(',') || recipients.includes(';')
      ? ['此链接包含多个收件人，请在发送前逐一核对']
      : []

    return {
      url: parsed.href,
      kind: 'mailto',
      primaryLabel: recipients.length > 240 ? `${recipients.slice(0, 239)}…` : recipients,
      secondaryLabel: '将交给 Windows 默认邮件应用处理',
      hasHiddenParameters: Boolean(parsed.search || parsed.hash),
      warnings,
      risk: warnings.length > 0 ? 'caution' : 'normal',
    }
  }

  throw new Error('仅允许打开 HTTPS 或邮件地址链接')
}
