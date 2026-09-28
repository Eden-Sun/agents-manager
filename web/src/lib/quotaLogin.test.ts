import test from 'node:test'
import assert from 'node:assert/strict'
import { shouldUseIdentityLogin } from './quotaLogin.ts'

test('quota login routes named codex and grok identities through the daemon', () => {
  assert.equal(shouldUseIdentityLogin('work'), true)
  assert.equal(shouldUseIdentityLogin('cc1'), true)
})

test('quota login keeps the shell fallback for default accounts', () => {
  assert.equal(shouldUseIdentityLogin(null), false)
})
