const allowedComposeTags = new Set([
  'a', 'blockquote', 'br', 'code', 'div', 'em', 'h1', 'h2', 'h3', 'img', 'li', 'ol', 'p', 'pre',
  's', 'strong', 'u', 'ul',
])
const removedComposeTags = 'script,style,iframe,object,embed,form,input,button,select,textarea,svg,math,template,link,meta'
const composeBlockTags = new Set(['BLOCKQUOTE', 'DIV', 'H1', 'H2', 'H3', 'LI', 'OL', 'P', 'PRE', 'UL'])
const originalMessageDivider = /\n{2,}(?=---------- (?:原始|转发)邮件 ----------)/u
const utf8Encoder = new TextEncoder()

export const MAX_COMPOSE_HTML_BYTES = 2 * 1024 * 1024

function escapeHtml(value: string) {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

function safeComposeHref(value: string) {
  const trimmed = value.trim()
  if (!trimmed || trimmed.length > 2_048 || /[\u0000-\u001f\u007f]/u.test(trimmed)) return undefined
  if (/^mailto:/i.test(trimmed)) {
    const normalized = trimmed.toLowerCase()
    try {
      const parsed = new URL(trimmed)
      return parsed.protocol === 'mailto:'
        && Boolean(parsed.pathname.trim())
        && !normalized.includes('%0d')
        && !normalized.includes('%0a')
        ? trimmed
        : undefined
    } catch {
      return undefined
    }
  }
  try {
    const parsed = new URL(trimmed)
    return parsed.protocol === 'https:' && !parsed.username && !parsed.password ? parsed.toString() : undefined
  } catch {
    return undefined
  }
}

export function normalizeComposeLink(value: string) {
  const trimmed = value.trim()
  const candidate = /^[a-z][a-z0-9+.-]*:/i.test(trimmed) ? trimmed : `https://${trimmed}`
  return safeComposeHref(candidate)
}

function replaceTag(element: Element, tagName: 'strong' | 'em' | 's') {
  const replacement = element.ownerDocument.createElement(tagName)
  while (element.firstChild) replacement.append(element.firstChild)
  element.replaceWith(replacement)
  return replacement
}

export function sanitizeComposeHtml(input: string) {
  const parsed = new DOMParser().parseFromString(input, 'text/html')
  parsed.body.querySelectorAll(removedComposeTags).forEach((element) => element.remove())
  const comments = parsed.createTreeWalker(parsed.body, NodeFilter.SHOW_COMMENT)
  const commentNodes: Comment[] = []
  while (comments.nextNode()) commentNodes.push(comments.currentNode as Comment)
  commentNodes.forEach((comment) => comment.remove())

  for (const sourceElement of Array.from(parsed.body.querySelectorAll('*'))) {
    let element = sourceElement
    const sourceTag = element.tagName.toLowerCase()
    if (!allowedComposeTags.has(sourceTag) && sourceTag !== 'b' && sourceTag !== 'i' && sourceTag !== 'strike') {
      element.replaceWith(...Array.from(element.childNodes))
      continue
    }
    if (sourceTag === 'b') element = replaceTag(element, 'strong')
    if (sourceTag === 'i') element = replaceTag(element, 'em')
    if (sourceTag === 'strike') element = replaceTag(element, 's')

    const tag = element.tagName.toLowerCase()
    for (const attribute of Array.from(element.attributes)) {
      const name = attribute.name.toLowerCase()
      if (tag === 'a' && name === 'href') {
        const href = safeComposeHref(attribute.value)
        if (href) element.setAttribute('href', href)
        else element.removeAttribute(attribute.name)
        continue
      }
      if (tag === 'img' && name === 'src') {
        const source = attribute.value.trim()
        if (/^cid:[a-z0-9._@-]{1,128}$/i.test(source)) element.setAttribute('src', source)
        else element.removeAttribute(attribute.name)
        continue
      }
      if (tag === 'img' && name === 'alt') {
        element.setAttribute('alt', attribute.value.slice(0, 255))
        continue
      }
      element.removeAttribute(attribute.name)
    }
    if (tag === 'a' && !element.hasAttribute('href')) element.replaceWith(...Array.from(element.childNodes))
    if (tag === 'img' && !element.hasAttribute('src')) element.remove()
  }
  return parsed.body.innerHTML
}

function plainTextForNode(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) return node.textContent ?? ''
  if (!(node instanceof HTMLElement)) return ''
  if (node.tagName === 'BR') return '\n'
  let content = Array.from(node.childNodes).map(plainTextForNode).join('')
  if (node.tagName === 'LI') {
    const parent = node.parentElement
    const marker = parent?.tagName === 'OL'
      ? `${Array.from(parent.children).indexOf(node) + 1}. `
      : '• '
    content = marker + content
  }
  return composeBlockTags.has(node.tagName) ? `${content}\n` : content
}

export function composeHtmlToPlainText(input: string) {
  const parsed = new DOMParser().parseFromString(sanitizeComposeHtml(input), 'text/html')
  return Array.from(parsed.body.childNodes)
    .map(plainTextForNode)
    .join('')
    .replace(/\u00a0/g, ' ')
    .replace(/[ \t]+\n/g, '\n')
    .replace(/\n{3,}/g, '\n\n')
    .trimEnd()
}

function textBlock(value: string, tag = 'div') {
  return `<${tag}>${escapeHtml(value).replace(/\n/g, '<br>')}</${tag}>`
}

export function plainTextToComposeHtml(input: string) {
  const normalized = input.replace(/\r\n?/g, '\n')
  if (!normalized) return ''
  const divider = normalized.search(originalMessageDivider)
  if (divider < 0) return textBlock(normalized)
  const written = normalized.slice(0, divider).trimEnd()
  const quoted = normalized.slice(divider).trimStart()
  return `${written ? textBlock(written) : '<div><br></div>'}${textBlock(quoted, 'blockquote')}`
}

export function appendSignatureToComposeHtml(input: string, signature: string) {
  const normalizedSignature = signature.replace(/\r\n?/g, '\n').trim()
  const safeInput = sanitizeComposeHtml(input)
  if (!normalizedSignature) return safeInput
  const parsed = new DOMParser().parseFromString(safeInput, 'text/html')
  const gap = parsed.createElement('div')
  gap.append(parsed.createElement('br'))
  const signatureBlock = parsed.createElement('div')
  signatureBlock.append('-- ')
  const signatureLines = normalizedSignature.split('\n')
  for (const [index, line] of signatureLines.entries()) {
    signatureBlock.append(parsed.createTextNode(line))
    if (index < signatureLines.length - 1) signatureBlock.append(parsed.createElement('br'))
  }
  const quoted = parsed.body.querySelector('blockquote')
  if (parsed.body.childNodes.length) parsed.body.insertBefore(gap, quoted)
  parsed.body.insertBefore(signatureBlock, quoted)
  return sanitizeComposeHtml(parsed.body.innerHTML)
}

export function composeHtmlBytes(value: string) {
  return utf8Encoder.encode(value).byteLength
}
