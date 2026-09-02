export const MAX_ACCOUNT_SIGNATURE_BYTES = 8 * 1024

const unsafeSignatureControl = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/u
const originalMessageDivider = /\n{2,}(?=---------- (?:原始|转发)邮件 ----------)/u
const utf8Encoder = new TextEncoder()

export function accountSignatureBytes(value: string) {
  return utf8Encoder.encode(value).byteLength
}

export function normalizeAccountSignature(value: string) {
  const normalized = value.replace(/\r\n?/g, '\n').trim()
  if (unsafeSignatureControl.test(normalized)) {
    throw new Error('签名包含不支持的控制字符')
  }
  if (accountSignatureBytes(normalized) > MAX_ACCOUNT_SIGNATURE_BYTES) {
    throw new Error('账户签名不能超过 8 KB')
  }
  return normalized
}

export function appendAccountSignature(body: string, signature: string) {
  const normalizedSignature = normalizeAccountSignature(signature)
  if (!normalizedSignature) return body

  const signatureBlock = `-- \n${normalizedSignature}`
  const divider = body.search(originalMessageDivider)
  if (divider >= 0) {
    const written = body.slice(0, divider).trimEnd()
    const quoted = body.slice(divider).trimStart()
    return `${written}${written ? '\n\n' : ''}${signatureBlock}\n\n${quoted}`
  }

  const written = body.trimEnd()
  return `${written}${written ? '\n\n' : ''}${signatureBlock}`
}
