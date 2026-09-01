import assert from 'node:assert/strict'
import { buildComposeThreadHeaders } from '../src/compose-thread.ts'
import { buildMailThreads } from '../src/threading.ts'

function mail(id, overrides = {}) {
  return {
    id,
    accountId: 'account-a',
    folder: 'inbox',
    nativeFolder: 'INBOX',
    from: `${id}@example.invalid`,
    senderName: id,
    subject: 'Project',
    preview: id,
    timestamp: id,
    dateGroup: '今天',
    unread: false,
    starred: false,
    accent: '#123456',
    avatar: id.slice(0, 2),
    body: [id],
    ...overrides,
  }
}

const conversation = buildMailThreads([
  mail('reply-2', { threadId: 'root@example.invalid', messageId: 'reply-2@example.invalid', receivedAt: '2026-09-01T03:00:00Z', unread: true }),
  mail('root', { threadId: 'root@example.invalid', messageId: 'root@example.invalid', receivedAt: '2026-09-01T01:00:00Z' }),
  mail('reply-1', { threadId: 'root@example.invalid', messageId: 'reply-1@example.invalid', receivedAt: '2026-09-01T02:00:00Z', unread: true }),
])
assert.equal(conversation.length, 1)
assert.equal(conversation[0].latest.id, 'reply-2')
assert.deepEqual(conversation[0].messages.map(({ id }) => id), ['root', 'reply-1', 'reply-2'])
assert.equal(conversation[0].unreadCount, 2)

const scoped = buildMailThreads([
  mail('account-a', { threadId: 'shared@example.invalid' }),
  mail('account-b', { accountId: 'account-b', threadId: 'shared@example.invalid' }),
  mail('sent-copy', { nativeFolder: 'Sent', threadId: 'shared@example.invalid' }),
])
assert.equal(scoped.length, 3)

const replyOnlyChain = buildMailThreads([
  mail('root-only', { messageId: 'root-only@example.invalid', threadId: 'root-only@example.invalid' }),
  mail('reply-only-1', { messageId: 'reply-only-1@example.invalid', inReplyTo: 'root-only@example.invalid', threadId: 'root-only@example.invalid' }),
  mail('reply-only-2', { messageId: 'reply-only-2@example.invalid', inReplyTo: 'reply-only-1@example.invalid', threadId: 'reply-only-1@example.invalid' }),
])
assert.equal(replyOnlyChain.length, 1)
assert.equal(replyOnlyChain[0].messages.length, 3)

const singletons = buildMailThreads([mail('one'), mail('two')])
assert.equal(singletons.length, 2)

const replyHeaders = buildComposeThreadHeaders('reply', mail('parent', {
  messageId: 'parent@example.invalid',
  inReplyTo: 'root@example.invalid',
  references: ['root@example.invalid'],
}))
assert.equal(replyHeaders.inReplyTo, 'parent@example.invalid')
assert.deepEqual(replyHeaders.references, ['root@example.invalid', 'parent@example.invalid'])

const parentOnlyHeaders = buildComposeThreadHeaders('reply-all', mail('parent-only', {
  messageId: 'parent-only@example.invalid',
  inReplyTo: 'root-only@example.invalid',
}))
assert.deepEqual(parentOnlyHeaders.references, ['root-only@example.invalid', 'parent-only@example.invalid'])
assert.deepEqual(buildComposeThreadHeaders('forward', mail('forward', { messageId: 'forward@example.invalid' })), { references: [] })

const longChain = Array.from({ length: 32 }, (_, index) => `ancestor-${index}@example.invalid`)
const boundedReply = buildComposeThreadHeaders('reply', mail('bounded', {
  messageId: 'parent-bounded@example.invalid',
  references: longChain,
}))
assert.equal(boundedReply.references.length, 32)
assert.equal(boundedReply.references[0], longChain[0])
assert.equal(boundedReply.references.at(-1), 'parent-bounded@example.invalid')
assert.deepEqual(buildComposeThreadHeaders('reply', mail('unsafe', {
  messageId: 'unsafe@example.invalid\r\nBcc: hidden@example.invalid',
})), { references: [] })

console.log('conversation threading checks passed')
