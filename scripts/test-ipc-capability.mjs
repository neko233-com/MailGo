import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import ts from 'typescript'

const sourcePath = new URL('../src/lib/ipc.ts', import.meta.url)
const source = await readFile(sourcePath, 'utf8')
const transpiled = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(transpiled).toString('base64')}`
const { readNativeCapability } = await import(moduleUrl)

const capability = 'Ab9'.repeat(16)

function location(overrides = {}) {
  return {
    protocol: 'rdesktop:',
    host: 'localhost',
    hash: `#ipc=${capability}`,
    pathname: '/index.html',
    search: '?density=compact',
    ...overrides,
  }
}

function historyRecorder() {
  const calls = []
  return {
    calls,
    history: {
      replaceState: (...args) => calls.push(args),
    },
  }
}

function storageRecorder(initialValue) {
  let value = initialValue
  const calls = []
  return {
    calls,
    storage: {
      getItem: (key) => {
        calls.push(['getItem', key])
        return value
      },
      setItem: (key, nextValue) => {
        calls.push(['setItem', key, nextValue])
        value = nextValue
      },
      removeItem: (key) => {
        calls.push(['removeItem', key])
        value = undefined
      },
    },
    value: () => value,
  }
}

for (const trustedLocation of [
  location(),
  location({ protocol: 'http:', host: 'rdesktop.localhost' }),
]) {
  const recorder = historyRecorder()
  const session = storageRecorder()
  assert.equal(readNativeCapability(trustedLocation, recorder.history, session.storage), capability)
  assert.deepEqual(recorder.calls, [[null, '', '/index.html?density=compact']])
  assert.equal(session.value(), capability)
  assert.equal(
    readNativeCapability(location({ ...trustedLocation, hash: '' }), recorder.history, session.storage),
    capability,
    'a trusted WebView reload must recover the process capability from session-only storage',
  )
}

const rotatedCapability = 'Zy8'.repeat(16)
const rotatedSession = storageRecorder(capability)
assert.equal(
  readNativeCapability(location({ hash: `#ipc=${rotatedCapability}` }), historyRecorder().history, rotatedSession.storage),
  rotatedCapability,
)
assert.equal(rotatedSession.value(), rotatedCapability, 'a new native process token must replace stale session state')

for (const untrustedLocation of [
  location({ protocol: 'https:', host: 'example.invalid' }),
  location({ protocol: 'http:', host: 'rdesktop.localhost.evil.invalid' }),
  location({ protocol: 'http:', host: 'rdesktop.localhost:1420' }),
  location({ protocol: 'rdesktop:', host: 'localhost.evil.invalid' }),
]) {
  const recorder = historyRecorder()
  const session = storageRecorder(capability)
  assert.equal(readNativeCapability(untrustedLocation, recorder.history, session.storage), undefined)
  assert.equal(recorder.calls.length, 0)
  assert.equal(session.calls.length, 0, 'untrusted origins must not read capability storage')
}

for (const malformedHash of [
  `#ipc=${'A'.repeat(47)}`,
  `#ipc=${'A'.repeat(49)}`,
  `#ipc=${'A'.repeat(47)}!`,
  `#other=value&ipc=${capability}`,
]) {
  const recorder = historyRecorder()
  const session = storageRecorder(capability)
  assert.equal(readNativeCapability(location({ hash: malformedHash }), recorder.history, session.storage), undefined)
  assert.equal(recorder.calls.length, 0)
  assert.equal(session.calls.length, 0, 'a malformed launch hash must fail closed instead of using stored state')
}

const failedScrub = { replaceState: () => { throw new Error('history unavailable') } }
const scrubFailureSession = storageRecorder()
assert.equal(readNativeCapability(location(), failedScrub, scrubFailureSession.storage), undefined)
assert.equal(scrubFailureSession.calls.length, 0, 'a capability must not be stored until its URL is scrubbed')

const malformedStored = storageRecorder('not-a-valid-capability')
assert.equal(readNativeCapability(location({ hash: '' }), historyRecorder().history, malformedStored.storage), undefined)
assert.deepEqual(malformedStored.calls.map((call) => call[0]), ['getItem', 'removeItem'])

const unavailableStorage = {
  getItem: () => { throw new Error('storage disabled') },
  setItem: () => { throw new Error('storage disabled') },
  removeItem: () => { throw new Error('storage disabled') },
}
assert.equal(readNativeCapability(location(), historyRecorder().history, unavailableStorage), capability)
assert.equal(readNativeCapability(location({ hash: '' }), historyRecorder().history, unavailableStorage), undefined)

console.log('packaged IPC capability launch, rotation, and trusted reload checks passed')
