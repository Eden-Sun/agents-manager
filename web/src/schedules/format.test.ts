import test from 'node:test'
import assert from 'node:assert/strict'
import { occurrenceLine } from './format'

test('only a queued turn waiting for startup displays its original startup error', () => {
  const o = { status: 'queued' as const, turn_status: 'queued', turn_awaits_start: 1, turn_start_error: 'AM_GROK_UNTRUSTED' }
  assert.match(occurrenceLine(o), /AM_GROK_UNTRUSTED/)
  assert.doesNotMatch(occurrenceLine(o, 'in_flight'), /AM_GROK_UNTRUSTED/)
  assert.doesNotMatch(occurrenceLine({ ...o, turn_awaits_start: 0 }), /AM_GROK_UNTRUSTED/)
  assert.doesNotMatch(occurrenceLine({ ...o, turn_start_error: null }), /AM_GROK_UNTRUSTED/)
})
