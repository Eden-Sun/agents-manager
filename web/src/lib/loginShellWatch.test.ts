import test from 'node:test'
import assert from 'node:assert/strict'
import { forgetLoginShell, isLoginShell, loginSucceeded, watchLoginShell } from './loginShellWatch'

test('登入成功的字樣', () => {
  assert.ok(loginSucceeded('2. Enter this one-time code\n   YP8Z-1QK6G\n\nSuccessfully logged in\n$ '))
  assert.ok(loginSucceeded('Login successful.'))
  assert.ok(loginSucceeded('✓ Logged in as hunta'))
  assert.ok(!loginSucceeded('$ codex login --device-auth\nFollow these steps to sign in'))
  assert.ok(!loginSucceeded('Not logged in'))
})

test('名單：空 host 等於 local，忘掉就不算', () => {
  watchLoginShell('', 'w1:p1')
  assert.ok(isLoginShell('local', 'w1:p1'))
  forgetLoginShell('local', 'w1:p1')
  assert.ok(!isLoginShell('', 'w1:p1'))
})
