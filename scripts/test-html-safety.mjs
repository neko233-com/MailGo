import assert from 'node:assert/strict'
import { isSafeRemoteImageUrl, preflightHtmlStructure } from '../src/htmlSafety.ts'

assert.equal(preflightHtmlStructure('<article><p>ordinary message</p><img src="cid:logo"></article>'), true)
assert.equal(preflightHtmlStructure(`${'<div>'.repeat(129)}body${'</div>'.repeat(129)}`), false)
assert.equal(preflightHtmlStructure('<div></unknown>'.repeat(129)), false)
assert.equal(preflightHtmlStructure('<br>'.repeat(20_001)), false)
assert.equal(preflightHtmlStructure(`<div data-value="${'x'.repeat(65 * 1024)}">body</div>`), false)
assert.equal(preflightHtmlStructure('x'.repeat(4 * 1024 * 1024 + 1)), false)

assert.equal(isSafeRemoteImageUrl('https://images.example.invalid/pixel.png'), true)
assert.equal(isSafeRemoteImageUrl('https://images.example.invalid:443/pixel.png'), true)
assert.equal(isSafeRemoteImageUrl('https://localhost/pixel.png'), false)
assert.equal(isSafeRemoteImageUrl('https://127.0.0.1/pixel.png'), false)
assert.equal(isSafeRemoteImageUrl('https://10.0.0.1/pixel.png'), false)
assert.equal(isSafeRemoteImageUrl('https://[::1]/pixel.png'), false)
assert.equal(isSafeRemoteImageUrl('https://[fd00::1]/pixel.png'), false)
assert.equal(isSafeRemoteImageUrl('https://name:token@images.example.invalid/pixel.png'), false)
assert.equal(isSafeRemoteImageUrl('https://images.example.invalid:8443/pixel.png'), false)
assert.equal(isSafeRemoteImageUrl('http://images.example.invalid/pixel.png'), false)

console.log('HTML structure and remote image safety checks passed')
