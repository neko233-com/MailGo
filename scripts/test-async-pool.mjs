import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { mapWithConcurrency } from '../src/lib/asyncPool.ts'

const wait = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds))

let active = 0
let peak = 0
const ordered = await mapWithConcurrency([18, 4, 12, 1, 8, 3], 3, async (delay, index) => {
  active += 1
  peak = Math.max(peak, active)
  await wait(delay)
  active -= 1
  return `result-${index}`
})
assert.equal(peak, 3, 'pool must use but never exceed the configured concurrency')
assert.deepEqual(ordered, ['result-0', 'result-1', 'result-2', 'result-3', 'result-4', 'result-5'])

const attempted = []
await assert.rejects(
  mapWithConcurrency([0, 1, 2, 3], 2, async (item) => {
    attempted.push(item)
    if (item === 1) throw new Error('fixture failure')
    await wait(1)
    return item
  }),
  /fixture failure/,
)
assert.deepEqual(attempted.toSorted((left, right) => left - right), [0, 1, 2, 3], 'one failure must not abandon the remaining bounded work')

await assert.rejects(mapWithConcurrency([1], 0, async (item) => item), RangeError)
assert.deepEqual(await mapWithConcurrency([], 2, async (item) => item), [])

const appSource = readFileSync(new URL('../src/App.tsx', import.meta.url), 'utf8')
assert.doesNotMatch(
  appSource,
  /Promise\.all\((?:accountList|accounts|nativeState\.accounts|pendingAccounts|selected)\.map/,
  'account and bulk IPC fan-out must stay behind the bounded pool',
)

console.log('bounded async pool checks passed')
