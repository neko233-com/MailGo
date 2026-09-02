import assert from 'node:assert/strict'
import {
  MAX_ACCOUNT_SIGNATURE_BYTES,
  accountSignatureBytes,
  appendAccountSignature,
  normalizeAccountSignature,
} from '../src/signature.ts'

assert.equal(normalizeAccountSignature('  MailGo\r\nDesktop  '), 'MailGo\nDesktop')
assert.throws(() => normalizeAccountSignature('unsafe\0signature'), /控制字符/)
assert.equal(accountSignatureBytes('签'), 3)
assert.equal(accountSignatureBytes('a'.repeat(MAX_ACCOUNT_SIGNATURE_BYTES)), MAX_ACCOUNT_SIGNATURE_BYTES)
assert.throws(
  () => normalizeAccountSignature('签'.repeat(MAX_ACCOUNT_SIGNATURE_BYTES)),
  /不能超过 8 KB/,
)

assert.equal(appendAccountSignature('Hello', ''), 'Hello')
assert.equal(
  appendAccountSignature('Hello', 'MailGo'),
  'Hello\n\n-- \nMailGo',
)
assert.equal(
  appendAccountSignature('Thanks\n\n---------- 原始邮件 ----------\nOriginal', 'MailGo'),
  'Thanks\n\n-- \nMailGo\n\n---------- 原始邮件 ----------\nOriginal',
)
assert.equal(
  appendAccountSignature('\n\n---------- 转发邮件 ----------\nForwarded', '姓名 <b>'),
  '-- \n姓名 <b>\n\n---------- 转发邮件 ----------\nForwarded',
)

console.log('account signature checks passed')
