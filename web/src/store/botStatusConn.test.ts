import test from 'node:test'
import assert from 'node:assert/strict'
import { botStatusConnTarget } from './botStatusConn.ts'

test('no connected field → touches nothing', () => {
  assert.deepEqual(botStatusConnTarget({ connected: undefined, host: 'local', defaultSession: false }), { kind: 'none' })
  assert.deepEqual(botStatusConnTarget({ connected: undefined, host: 'mac_builder', defaultSession: false }), { kind: 'none' })
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
  assert.deepEqual(botStatusConnTarget({ connected: false, host: 'mac_builder', defaultSession: false }), { kind: 'host', host: 'mac_builder', connected: false })
  assert.deepEqual(botStatusConnTarget({ connected: true, host: 'mac_builder', defaultSession: true }), { kind: 'host', host: 'mac_builder', connected: true })
})
