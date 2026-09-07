import test from 'node:test'
import assert from 'node:assert/strict'
import { botStatusConnTarget } from './botStatusConn.ts'

test('no connected field → touches nothing', () => {
  assert.deepEqual(botStatusConnTarget({ connected: undefined, host: 'local', defaultSession: false }), { kind: 'none' })
  assert.deepEqual(botStatusConnTarget({ connected: undefined, host: 'm4p', defaultSession: false }), { kind: 'none' })
})

test('local manager-session bot writes the global flag', () => {
  assert.deepEqual(botStatusConnTarget({ connected: false, host: 'local', defaultSession: false }), { kind: 'global', connected: false })
  // empty host means local
  assert.deepEqual(botStatusConnTarget({ connected: true, host: '', defaultSession: false }), { kind: 'global', connected: true })
})

test('local default-session bot writes defaultConnected only', () => {
  assert.deepEqual(botStatusConnTarget({ connected: false, host: 'local', defaultSession: true }), { kind: 'default', connected: false })
})

test('remote bot never touches the global flag, regardless of session (#20)', () => {
  assert.deepEqual(botStatusConnTarget({ connected: false, host: 'm4p', defaultSession: false }), { kind: 'host', host: 'm4p', connected: false })
  assert.deepEqual(botStatusConnTarget({ connected: true, host: 'm4p', defaultSession: true }), { kind: 'host', host: 'm4p', connected: true })
})
