import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import ts from 'typescript'

const root = resolve(import.meta.dirname, '..')
const source = readFileSync(resolve(root, 'src/messageHydration.ts'), 'utf8')
const app = readFileSync(resolve(root, 'src/App.tsx'), 'utf8')
const release = readFileSync(resolve(root, 'scripts/release-windows.ps1'), 'utf8')
const output = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2022, target: ts.ScriptTarget.ES2022 },
}).outputText
const implementation = await import(`data:text/javascript;base64,${Buffer.from(output).toString('base64')}`)

const message = (id, overrides = {}) => ({
  id,
  accountId: 'account-a',
  folder: 'inbox',
  from: 'sender@example.invalid',
  senderName: 'Sender',
  subject: id,
  preview: id,
  timestamp: '08:00',
  dateGroup: 'today',
  unread: false,
  starred: false,
  accent: '#123456',
  avatar: 'S',
  body: ['正在加载邮件正文…'],
  nativeUid: Number(id.replace(/\D/g, '')) + 1,
  nativeFolder: 'INBOX',
  ...overrides,
})

const mails = Array.from({ length: 9 }, (_, index) => message(`mail-${index}`))
assert.deepEqual(
  implementation.selectBodyHydrationCandidates(mails, 'mail-4').map((mail) => mail.id),
  ['mail-4', 'mail-5', 'mail-3', 'mail-6'],
  'selected message must hydrate first, followed by bounded adjacent read-ahead',
)
assert.deepEqual(
  implementation.selectBodyHydrationCandidates(mails, 'mail-4', 99).map((mail) => mail.id),
  ['mail-4', 'mail-5', 'mail-3', 'mail-6'],
  'callers cannot exceed the production read-ahead limit',
)
assert.deepEqual(
  implementation.selectBodyHydrationCandidates(mails, 'mail-0').map((mail) => mail.id),
  ['mail-0', 'mail-1', 'mail-2'],
  'read-ahead must stay in range at a list boundary',
)

const mixed = [
  message('mail-1', { body: ['Already cached'] }),
  message('mail-2'),
  message('mail-3', { nativeUid: undefined }),
  message('mail-4', { outboxId: 'queued-1' }),
  message('mail-5', { accountId: 'account-b', nativeUid: 2 }),
  message('mail-6', { accountId: 'account-b', nativeUid: 2, nativeFolder: 'inbox' }),
]
assert.deepEqual(
  implementation.selectBodyHydrationCandidates(mixed, 'mail-2').map((mail) => mail.id),
  ['mail-2'],
  'cached, non-native, outbox, and out-of-radius rows must be skipped',
)
const duplicateIdentity = [
  message('mail-10', { nativeUid: 10 }),
  message('mail-11', { accountId: 'account-b', nativeUid: 22, nativeFolder: 'INBOX' }),
  message('mail-12', { accountId: 'account-b', nativeUid: 22, nativeFolder: 'inbox' }),
]
assert.deepEqual(
  implementation.selectBodyHydrationCandidates(duplicateIdentity, 'mail-10').map((mail) => mail.id),
  ['mail-10', 'mail-11'],
  'one native message identity must never be prefetched twice',
)
assert.equal(implementation.mailNeedsBodyHydration(message('mail-7')), true)
assert.equal(implementation.mailNeedsBodyHydration(message('mail-7', { htmlBody: '<p>Cached</p>', body: ['Cached'] })), false)
assert.deepEqual(implementation.selectBodyHydrationCandidates(mails, 'missing'), [])
assert.deepEqual(implementation.selectBodyHydrationCandidates(mails, 'mail-4', Number.NaN), [])

assert.match(app, /selectBodyHydrationCandidates\(visibleMails, selectedMail\.id\)/)
assert.match(app, /requestIdleCallback/)
assert.match(app, /mailNeedsBodyHydration\(mail\)/)
assert.match(app, /const accountsById = useMemo\(\(\) => new Map/)
assert.match(app, /const unreadNativeFolderCounts = useMemo/)
assert.match(release, /npm run test:message-hydration/)

console.log('Bounded body hydration and hot-path account index checks passed.')
