const MAX_HTML_INPUT_BYTES = 4 * 1024 * 1024
const MAX_HTML_ELEMENTS = 20_000
const MAX_HTML_ATTRIBUTES = 80_000
const MAX_HTML_ATTRIBUTE_BYTES = 1024 * 1024
const MAX_HTML_NESTING_DEPTH = 128
const MAX_HTML_TAG_BYTES = 64 * 1024
const MAX_REMOTE_IMAGES = 32
const MAX_UNIQUE_REMOTE_IMAGE_URLS = 16

export const HTML_BLOCKED_FALLBACK = '<p class="mail-html-blocked">这封邮件的 HTML 结构过于复杂，已为安全和性能自动拦截。请切换到纯文本查看。</p>'

const HTML_PRESENTATIONAL_SIZING_ATTRIBUTES = new Set([
  'border', 'cellpadding', 'cellspacing', 'face', 'height', 'nowrap', 'size', 'width',
])

const VOID_HTML_TAGS = new Set([
  'area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta', 'param', 'source',
  'track', 'wbr',
])

function utf8Bytes(input: string) {
  let bytes = 0
  for (let index = 0; index < input.length; index += 1) {
    const code = input.charCodeAt(index)
    if (code <= 0x7f) bytes += 1
    else if (code <= 0x7ff) bytes += 2
    else if (code >= 0xd800 && code <= 0xdbff && index + 1 < input.length && input.charCodeAt(index + 1) >= 0xdc00 && input.charCodeAt(index + 1) <= 0xdfff) {
      bytes += 4
      index += 1
    } else bytes += 3
  }
  return bytes
}

function findTagEnd(input: string, start: number) {
  let quote = ''
  for (let index = start; index < input.length; index += 1) {
    const character = input[index]
    if (quote) {
      if (character === quote) quote = ''
    } else if (character === '"' || character === "'") {
      quote = character
    } else if (character === '>') {
      return index
    }
  }
  return -1
}

function attributeComplexity(input: string) {
  let count = 0
  let bytes = 0
  let cursor = 0
  while (cursor < input.length) {
    while (/\s/u.test(input[cursor] ?? '')) cursor += 1
    if (cursor >= input.length || input[cursor] === '/') break
    const start = cursor
    while (cursor < input.length && !/[\s=/>]/u.test(input[cursor])) cursor += 1
    if (cursor === start) {
      cursor += 1
      continue
    }
    while (/\s/u.test(input[cursor] ?? '')) cursor += 1
    if (input[cursor] === '=') {
      cursor += 1
      while (/\s/u.test(input[cursor] ?? '')) cursor += 1
      const quote = input[cursor] === '"' || input[cursor] === "'" ? input[cursor++] : ''
      if (quote) {
        const end = input.indexOf(quote, cursor)
        if (end < 0) return null
        cursor = end + 1
      } else {
        while (cursor < input.length && !/[\s>]/u.test(input[cursor])) cursor += 1
      }
    }
    count += 1
    bytes += utf8Bytes(input.slice(start, cursor))
  }
  return { count, bytes }
}

export function preflightHtmlStructure(input: string) {
  if (utf8Bytes(input) > MAX_HTML_INPUT_BYTES) return false
  let elements = 0
  let attributes = 0
  let attributeBytes = 0
  const openTags: string[] = []
  let cursor = 0

  while (cursor < input.length) {
    const start = input.indexOf('<', cursor)
    if (start < 0) break
    if (input.startsWith('<!--', start)) {
      const end = input.indexOf('-->', start + 4)
      if (end < 0) return false
      cursor = end + 3
      continue
    }
    const end = findTagEnd(input, start + 1)
    if (end < 0 || utf8Bytes(input.slice(start, end + 1)) > MAX_HTML_TAG_BYTES) return false
    cursor = end + 1
    const inner = input.slice(start + 1, end).trim()
    if (!inner || inner.startsWith('!') || inner.startsWith('?')) continue
    if (inner.startsWith('/')) {
      const closingName = inner.slice(1).trimStart().match(/^[A-Za-z0-9:-]+/u)?.[0]?.toLowerCase()
      if (closingName && openTags.at(-1) === closingName) openTags.pop()
      continue
    }
    const name = inner.match(/^[A-Za-z0-9:-]+/u)?.[0]
    if (!name) continue
    elements += 1
    if (elements > MAX_HTML_ELEMENTS) return false
    const complexity = attributeComplexity(inner.slice(name.length))
    if (!complexity) return false
    attributes += complexity.count
    attributeBytes += complexity.bytes
    if (attributes > MAX_HTML_ATTRIBUTES || attributeBytes > MAX_HTML_ATTRIBUTE_BYTES) return false
    if (!inner.endsWith('/') && !VOID_HTML_TAGS.has(name.toLowerCase())) {
      openTags.push(name.toLowerCase())
      if (openTags.length > MAX_HTML_NESTING_DEPTH) return false
    }
  }
  return true
}

function parseIpv4(hostname: string) {
  const parts = hostname.split('.')
  if (parts.length !== 4 || parts.some((part) => !/^\d{1,3}$/u.test(part))) return null
  const octets = parts.map(Number)
  return octets.every((octet) => octet >= 0 && octet <= 255) ? octets : null
}

function isPublicIpv4(octets: number[]) {
  const [first, second, third, fourth] = octets
  return first !== 0
    && first !== 10
    && first !== 127
    && !(first === 169 && second === 254)
    && !(first === 172 && second >= 16 && second <= 31)
    && !(first === 192 && second === 168)
    && !(first === 100 && second >= 64 && second <= 127)
    && !(first === 192 && second === 0 && third === 0)
    && !(first === 198 && (second === 18 || second === 19))
    && first < 224
    && !(first === 255 && second === 255 && third === 255 && fourth === 255)
}

function parseIpv6(hostname: string) {
  if (!hostname.includes(':') || hostname.includes('%')) return null
  const halves = hostname.split('::')
  if (halves.length > 2) return null
  const left = halves[0] ? halves[0].split(':') : []
  const right = halves[1] ? halves[1].split(':') : []
  if (halves.length === 1 && left.length !== 8) return null
  const missing = 8 - left.length - right.length
  if (missing < (halves.length === 2 ? 1 : 0)) return null
  const parts = [...left, ...Array(missing).fill('0'), ...right]
  if (parts.length !== 8 || parts.some((part) => !/^[0-9a-f]{1,4}$/iu.test(part))) return null
  return parts.map((part) => Number.parseInt(part, 16))
}

function isPublicIpv6(parts: number[]) {
  const first = parts[0]
  if (parts.every((part) => part === 0) || parts.slice(0, 7).every((part) => part === 0) && parts[7] === 1) return false
  if ((first & 0xfe00) === 0xfc00 || (first & 0xffc0) === 0xfe80 || (first & 0xff00) === 0xff00) return false
  if (parts.slice(0, 5).every((part) => part === 0) && (parts[5] === 0 || parts[5] === 0xffff)) {
    return isPublicIpv4([parts[6] >> 8, parts[6] & 0xff, parts[7] >> 8, parts[7] & 0xff])
  }
  return true
}

export function isSafeRemoteImageUrl(value: string) {
  let url: URL
  try {
    url = new URL(value)
  } catch {
    return false
  }
  if (url.protocol !== 'https:' || url.username || url.password || (url.port && url.port !== '443')) return false
  const hostname = url.hostname.replace(/^\[|\]$/gu, '').replace(/\.$/u, '').toLowerCase()
  if (!hostname || hostname === 'localhost' || hostname.endsWith('.localhost') || hostname.includes('%')) return false
  const ipv4 = parseIpv4(hostname)
  if (ipv4) return isPublicIpv4(ipv4)
  if (hostname.includes(':')) {
    const ipv6 = parseIpv6(hostname)
    return ipv6 ? isPublicIpv6(ipv6) : false
  }
  return true
}

export function sanitizeHtml(input: string, allowRemoteImages = false) {
  if (!preflightHtmlStructure(input) || typeof DOMParser === 'undefined') return HTML_BLOCKED_FALLBACK
  const documentParser = new DOMParser().parseFromString(input, 'text/html')
  documentParser.querySelectorAll('script, iframe, object, embed, form, link, meta, style').forEach((node) => node.remove())
  const nodes = documentParser.querySelectorAll('*')
  if (nodes.length > MAX_HTML_ELEMENTS) return HTML_BLOCKED_FALLBACK
  let attributeCount = 0
  let attributeBytes = 0
  let remoteImageCount = 0
  const remoteImageUrls = new Set<string>()
  for (const node of nodes) {
    for (const attribute of Array.from(node.attributes)) {
      attributeCount += 1
      attributeBytes += utf8Bytes(attribute.name) + utf8Bytes(attribute.value)
      if (attributeCount > MAX_HTML_ATTRIBUTES || attributeBytes > MAX_HTML_ATTRIBUTE_BYTES) return HTML_BLOCKED_FALLBACK
      const name = attribute.name.toLowerCase()
      const value = attribute.value.trim().replace(/[\u0000-\u0020]+/gu, '')
      const inlineImage = name === 'src' && /^(cid:|data:image\/(?:png|gif|jpe?g|webp);base64,)/iu.test(value)
      let safeRemoteImage = false
      if (name === 'src' && allowRemoteImages && isSafeRemoteImageUrl(value) && remoteImageCount < MAX_REMOTE_IMAGES) {
        const normalized = new URL(value).href
        if (remoteImageUrls.has(normalized) || remoteImageUrls.size < MAX_UNIQUE_REMOTE_IMAGE_URLS) {
          safeRemoteImage = true
          remoteImageCount += 1
          remoteImageUrls.add(normalized)
        }
      }
      const isSafeUrl = name === 'href'
        ? /^(https:\/\/|mailto:|#)/iu.test(value)
        : inlineImage || safeRemoteImage
      if (name.startsWith('on') || ['style', 'srcdoc', 'srcset', 'ping', 'formaction', 'xlink:href'].includes(name) || HTML_PRESENTATIONAL_SIZING_ATTRIBUTES.has(name)) node.removeAttribute(attribute.name)
      if (['href', 'src', 'action'].includes(name) && !isSafeUrl) node.removeAttribute(attribute.name)
    }
    if (node instanceof HTMLImageElement && node.hasAttribute('src')) {
      node.loading = 'lazy'
      node.decoding = 'async'
      node.referrerPolicy = 'no-referrer'
    }
  }
  documentParser.querySelectorAll('a').forEach((node) => {
    if (node.getAttribute('target') === '_blank') node.setAttribute('rel', 'noreferrer noopener')
    else node.removeAttribute('target')
  })
  return documentParser.body.innerHTML
}
