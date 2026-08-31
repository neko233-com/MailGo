import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import ts from 'typescript'

const sourcePath = new URL('../src/customCss.ts', import.meta.url)
const source = await readFile(sourcePath, 'utf8')
const transpiled = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(transpiled).toString('base64')}`
const { sanitizeCustomCss } = await import(moduleUrl)

const safeTheme = sanitizeCustomCss(`:root { --accent: #ff8a67; }\n@media (max-width: 700px) { .app-shell { --density: compact; } }`)
assert.equal(safeTheme.removedUnsafeSyntax, false)
assert.match(safeTheme.css, /--accent: #ff8a67/)
assert.match(safeTheme.css, /@media/)

const blockedResources = sanitizeCustomCss('@import url("https://tracker.invalid/theme.css"); .app-shell { background: url(https://tracker.invalid/pixel); }')
assert.equal(blockedResources.removedUnsafeSyntax, true)
assert.doesNotMatch(blockedResources.css, /@import|url\s*\(/i)

const blockedLegacySyntax = sanitizeCustomCss('.legacy { behavior: url(#default#VML); -moz-binding: url(https://tracker.invalid/x); }')
assert.equal(blockedLegacySyntax.removedUnsafeSyntax, true)
assert.doesNotMatch(blockedLegacySyntax.css, /behavior|-moz-binding|url\s*\(/i)

const escapedImport = sanitizeCustomCss('@im\\port url("https://tracker.invalid/theme.css"); .app-shell { animation: fade-in .2s ease; }')
assert.equal(escapedImport.removedUnsafeSyntax, true)
assert.doesNotMatch(escapedImport.css, /@import|url\s*\(/i)
assert.match(escapedImport.css, /animation: fade-in/)

console.log('custom CSS sanitizer checks passed')
