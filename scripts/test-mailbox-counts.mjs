import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import ts from 'typescript'

const source = await readFile(new URL('../src/mailboxCounts.ts', import.meta.url), 'utf8')
const appSource = await readFile(new URL('../src/App.tsx', import.meta.url), 'utf8')
const releaseSource = await readFile(new URL('./release-windows.ps1', import.meta.url), 'utf8')
const transpiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(transpiled).toString('base64')}`
const { buildMailboxCountIndex, nativeFolderCountKey, nativeFolderName } = await import(moduleUrl)

const account = (id, provider) => ({ id, provider, label: provider, email: `${id}@example.invalid`, unread: 0, accent: '#888', status: 'synced', lastSync: '', signature: '' })
const accounts = [account('google', 'google'), account('qq', 'qq'), account('outlook', 'outlook')]
const mail = (id, accountId, folder, overrides = {}) => ({
  id,
  accountId,
  folder: 'inbox',
  nativeFolder: folder,
  from: 'sender@example.invalid',
  senderName: 'Sender',
  subject: id,
  preview: '',
  timestamp: '',
  dateGroup: 'Today',
  unread: true,
  starred: false,
  accent: '#888',
  avatar: 'S',
  body: [],
  ...overrides,
})

const messages = [
  mail('inbox', 'qq', 'INBOX'),
  mail('sent', 'qq', 'Sent Messages'),
  mail('archive', 'google', '[Gmail]/All Mail'),
  mail('spam-starred', 'outlook', 'Junk Email', { starred: true }),
  mail('trash', 'outlook', 'Deleted Items'),
  mail('blocked', 'qq', 'INBOX', { blocked: true }),
  mail('snoozed', 'qq', 'INBOX', { snoozedUntil: 1_900_000_000 }),
  mail('read', 'qq', 'INBOX', { unread: false }),
  mail('custom', 'qq', 'Receipts'),
  mail('draft', 'qq', undefined, { folder: 'drafts' }),
]

let iterations = 0
const onePassMessages = new Proxy(messages, {
  get(target, property, receiver) {
    if (property === Symbol.iterator) return function* iterateOnce() { iterations += 1; yield* target }
    return Reflect.get(target, property, receiver)
  },
})
const countIndex = buildMailboxCountIndex(onePassMessages, accounts)
const counts = countIndex.fixedUnread

assert.equal(iterations, 1, 'folder counters must inspect the message collection once')
assert.equal(counts.get('inbox'), 1)
assert.equal(counts.get('sent'), 1)
assert.equal(counts.get('archive'), 1)
assert.equal(counts.get('spam'), 1)
assert.equal(counts.get('trash'), 1)
assert.equal(counts.get('starred'), 1)
assert.equal(counts.has('drafts'), false)
assert.equal(countIndex.nativeUnread.get(nativeFolderCountKey('qq', 'INBOX')), 1)
assert.equal(countIndex.nativeUnread.has(nativeFolderCountKey('qq', 'Receipts')), true)
assert.equal(countIndex.hiddenUnreadByAccount.get('qq'), 2)
assert.equal(nativeFolderName(accounts[0], 'sent'), '[Gmail]/Sent Mail')
assert.equal(nativeFolderName(accounts[1], 'sent'), 'Sent Messages')
assert.equal(nativeFolderName(accounts[2], 'trash'), 'Deleted Items')
assert.match(appSource, /const mailboxCountIndex = useMemo\(\(\) => buildMailboxCountIndex\(allMails, accounts\)/)
assert.doesNotMatch(appSource, /const unreadNativeFolderCounts = useMemo/)
assert.match(appSource, /const visibleMailsById = useMemo\(\(\) => new Map/)
assert.match(appSource, /const visibleThreadsByMessageId = useMemo/)
assert.match(appSource, /const selectedMailIdSet = useMemo\(\(\) => new Set\(selectedMailIds\)/)
assert.doesNotMatch(appSource, /selectedMailIds\.includes\(mail\.id\)/)
assert.match(releaseSource, /npm run test:mailbox-counts/)

console.log('single-pass fixed-folder counters and provider mappings passed')
