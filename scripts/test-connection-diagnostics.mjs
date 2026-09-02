import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const mainSource = readFileSync(new URL('../native/src/main.rs', import.meta.url), 'utf8')
const syncSource = readFileSync(new URL('../native/src/sync.rs', import.meta.url), 'utf8')
const sendSource = readFileSync(new URL('../native/src/send.rs', import.meta.url), 'utf8')
const appSource = readFileSync(new URL('../src/App.tsx', import.meta.url), 'utf8')

function sourceSlice(source, startMarker, endMarker) {
  const start = source.indexOf(startMarker)
  assert.notEqual(start, -1, `missing source marker: ${startMarker}`)
  const end = source.indexOf(endMarker, start + startMarker.length)
  assert.notEqual(end, -1, `missing source marker: ${endMarker}`)
  return source.slice(start, end)
}

const route = sourceSlice(mainSource, '"accounts.diagnose" => {', '"accounts.export" => {')
assert.match(route, /try_begin_account_sync/)
assert.match(route, /std::thread::scope/)
assert.match(route, /sync::test_connection/)
assert.match(route, /send::test_connection/)
assert.doesNotMatch(route, /error\.to_string\(\)/)
assert.doesNotMatch(route, /send_message/)

const imapDiagnostic = sourceSlice(syncSource, 'pub fn test_connection(', 'fn body_session_fingerprint(')
assert.match(imapDiagnostic, /authenticate\(/)
assert.match(imapDiagnostic, /\.noop\(\)/)
assert.doesNotMatch(imapDiagnostic, /\.select\(/)
assert.doesNotMatch(imapDiagnostic, /fetch|download/i)

const smtpDiagnostic = sourceSlice(sendSource, 'pub fn test_connection(', 'fn build_message(')
assert.match(smtpDiagnostic, /SMTP_DIAGNOSTIC_TIMEOUT/)
assert.match(smtpDiagnostic, /\.test_connection\(\)/)
assert.doesNotMatch(smtpDiagnostic, /\.send\(/)
assert.doesNotMatch(smtpDiagnostic, /build_message/)

assert.match(appSource, /invoke<NativeConnectionDiagnostic>\('accounts\.diagnose'/)
assert.match(appSource, /只登录并发送 NOOP，不会发送邮件/)
assert.match(appSource, /connectionDiagnostics\[editingAccountId\]/)

console.log('Connection diagnostics are parallel, account-scoped, privacy-safe, and non-sending.')
