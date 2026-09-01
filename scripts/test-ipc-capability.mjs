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

for (const trustedLocation of [
  location(),
  location({ protocol: 'http:', host: 'rdesktop.localhost' }),
]) {
  const recorder = historyRecorder()
  assert.equal(readNativeCapability(trustedLocation, recorder.history), capability)
  assert.deepEqual(recorder.calls, [[null, '', '/index.html?density=compact']])
}

for (const untrustedLocation of [
  location({ protocol: 'https:', host: 'example.invalid' }),
  location({ protocol: 'http:', host: 'rdesktop.localhost.evil.invalid' }),
  location({ protocol: 'http:', host: 'rdesktop.localhost:1420' }),
  location({ protocol: 'rdesktop:', host: 'localhost.evil.invalid' }),
]) {
  const recorder = historyRecorder()
  assert.equal(readNativeCapability(untrustedLocation, recorder.history), undefined)
  assert.equal(recorder.calls.length, 0)
}

for (const malformedHash of [
  `#ipc=${'A'.repeat(47)}`,
  `#ipc=${'A'.repeat(49)}`,
  `#ipc=${'A'.repeat(47)}!`,
  `#other=value&ipc=${capability}`,
]) {
  const recorder = historyRecorder()
  assert.equal(readNativeCapability(location({ hash: malformedHash }), recorder.history), undefined)
  assert.equal(recorder.calls.length, 0)
}

const failedScrub = { replaceState: () => { throw new Error('history unavailable') } }
assert.equal(readNativeCapability(location(), failedScrub), undefined)

console.log('packaged IPC capability checks passed')
