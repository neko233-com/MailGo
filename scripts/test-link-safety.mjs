import assert from 'node:assert/strict'
import { inspectExternalLink, MAX_EXTERNAL_URL_BYTES } from '../src/linkSafety.ts'

const normal = inspectExternalLink('https://account.example.invalid/security?source=mail#notice', '检查账户活动')
assert.equal(normal.kind, 'https')
assert.equal(normal.primaryLabel, 'account.example.invalid')
assert.equal(normal.secondaryLabel, '/security')
assert.equal(normal.hasHiddenParameters, true)
assert.equal(normal.risk, 'normal')
assert.deepEqual(normal.warnings, [])

const mismatch = inspectExternalLink('https://actual.example.invalid/login', 'https://trusted.example.test/login')
assert.equal(mismatch.risk, 'caution')
assert.match(mismatch.warnings.join(' '), /显示文字.*实际将打开/u)

const relatedSubdomain = inspectExternalLink('https://login.example.invalid/', 'example.invalid')
assert.equal(relatedSubdomain.risk, 'normal')

const punycode = inspectExternalLink('https://xn--e1awd7f.invalid/', '打开网站')
assert.match(punycode.warnings.join(' '), /Punycode/u)

const ipAddress = inspectExternalLink('https://127.0.0.1/', '本地页面')
assert.match(ipAddress.warnings.join(' '), /IP 地址/u)

const unusualPort = inspectExternalLink('https://example.invalid:8443/', '打开网站')
assert.match(unusualPort.warnings.join(' '), /8443/u)

const email = inspectExternalLink('mailto:person@example.invalid?subject=Hello', '发送邮件')
assert.equal(email.kind, 'mailto')
assert.equal(email.primaryLabel, 'person@example.invalid')
assert.equal(email.hasHiddenParameters, true)

const multipleRecipients = inspectExternalLink('mailto:first@example.invalid,second@example.invalid')
assert.match(multipleRecipients.warnings.join(' '), /多个收件人/u)

assert.throws(() => inspectExternalLink('http://example.invalid/'), /仅允许/u)
assert.throws(() => inspectExternalLink('javascript:alert(1)'), /仅允许/u)
assert.throws(() => inspectExternalLink('https://name:token@example.invalid/'), /嵌入凭据/u)
assert.throws(() => inspectExternalLink('mailto:person@example.invalid?body=%0Aunsafe'), /换行注入/u)
assert.throws(() => inspectExternalLink('mailto:person%E2%80%AE@example.invalid'), /有效收件人/u)
assert.throws(() => inspectExternalLink(`https://example.invalid/${'a'.repeat(MAX_EXTERNAL_URL_BYTES)}`), /过长/u)
assert.throws(() => inspectExternalLink('https://example.invalid/\nunsafe'), /不安全字符/u)

console.log('external link safety checks passed')
