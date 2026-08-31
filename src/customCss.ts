const CSS_COMMENT_PATTERN = /\/\*[\s\S]*?\*\//g
const CSS_IMPORT_PATTERN = /@import\b[^;{}]*(?:;|$)/gi
const CSS_URL_PATTERN = /\burl\s*\([^)]*\)/gi
const CSS_EXPRESSION_PATTERN = /\bexpression\s*\([^)]*\)/gi
const CSS_SCRIPT_URL_PATTERN = /\b(?:javascript|vbscript)\s*:[^;{}\s]*/gi
const CSS_BEHAVIOR_PATTERN = /(^|[;{])([\t\r\n ]*)(?:behavior|-moz-binding)\s*:[^;{}]*(?=;|})/gim

export type SanitizedCustomCss = {
  css: string
  removedUnsafeSyntax: boolean
}

/**
 * Keep user CSS useful for themes while preventing it from becoming a hidden
 * network/resource loader. CSS is not an execution sandbox, but url/import and
 * legacy behavior features can still violate MailGo's offline/privacy policy.
 */
export function sanitizeCustomCss(input: string): SanitizedCustomCss {
  const normalized = input.replace(/\u0000/g, '').replace(/\\/g, '')
  const withoutComments = normalized.replace(CSS_COMMENT_PATTERN, '')
  let removedUnsafeSyntax = normalized !== input || withoutComments !== normalized
  let css = withoutComments

  const replaceUnsafe = (pattern: RegExp, replacement = '') => {
    const next = css.replace(pattern, replacement)
    if (next !== css) removedUnsafeSyntax = true
    css = next
  }

  replaceUnsafe(CSS_IMPORT_PATTERN)
  replaceUnsafe(CSS_URL_PATTERN)
  replaceUnsafe(CSS_EXPRESSION_PATTERN)
  replaceUnsafe(CSS_SCRIPT_URL_PATTERN)
  replaceUnsafe(CSS_BEHAVIOR_PATTERN, '$1$2')

  return { css, removedUnsafeSyntax }
}
