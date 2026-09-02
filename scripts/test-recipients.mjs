import assert from 'node:assert/strict'
import {
  activeRecipientQuery,
  applyRecipientSuggestion,
  filterRecipientDirectory,
  formatRecipientSuggestion,
  isSafeSuggestedEmail,
  recipientEmails,
} from '../src/recipients.ts'

const alice = { name: 'Alice Chen', email: 'alice@example.invalid', frequency: 8, lastSeen: '2026-08-01T00:00:00Z' }
const alex = { name: 'Alex Morgan', email: 'alex@example.invalid', frequency: 3, lastSeen: '2026-07-01T00:00:00Z' }

assert.equal(activeRecipientQuery('ali'), 'ali')
assert.equal(activeRecipientQuery('first@example.invalid,  ali'), 'ali')
assert.equal(activeRecipientQuery('first@example.invalid; second'), 'second')
assert.equal(activeRecipientQuery('first@example.invalid, '), '')
assert.equal(activeRecipientQuery('x'.repeat(400)).length, 256)

assert.deepEqual(
  [...recipientEmails('Alice <alice@example.invalid>, bob@example.invalid; partial@')].sort(),
  ['alice@example.invalid', 'bob@example.invalid', 'partial@'],
)
assert.equal(isSafeSuggestedEmail('alice@example.invalid'), true)
assert.equal(isSafeSuggestedEmail('alice @example.invalid'), false)
assert.equal(isSafeSuggestedEmail('alice@example'), false)
assert.equal(isSafeSuggestedEmail('alice@example.invalid\r\nBcc:x@example.invalid'), false)

assert.equal(formatRecipientSuggestion(alice), 'Alice Chen <alice@example.invalid>')
assert.equal(
  formatRecipientSuggestion({ ...alice, name: 'Doe, Alice\u202E' }),
  'Doe Alice <alice@example.invalid>',
)
assert.throws(
  () => formatRecipientSuggestion({ ...alice, email: 'unsafe address' }),
  /地址无效/u,
)

assert.equal(
  applyRecipientSuggestion('', alice),
  'Alice Chen <alice@example.invalid>, ',
)
assert.equal(
  applyRecipientSuggestion('first@example.invalid, al', alice),
  'first@example.invalid, Alice Chen <alice@example.invalid>, ',
)
assert.equal(
  applyRecipientSuggestion('first@example.invalid;al', alex),
  'first@example.invalid; Alex Morgan <alex@example.invalid>, ',
)

assert.deepEqual(
  filterRecipientDirectory([alex, alice], 'alice chen', new Set(), 8).map((item) => item.email),
  ['alice@example.invalid'],
)
assert.deepEqual(
  filterRecipientDirectory([alex, alice], 'al', new Set(['alice@example.invalid']), 8).map((item) => item.email),
  ['alex@example.invalid'],
)

console.log('recipient autocomplete checks passed')
