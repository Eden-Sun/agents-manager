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

test('側欄短時間：同時區只有月日時分；不同時區補縮寫', async () => {
  const { formatDueShort, defaultTimeZone } = await import('./format')
  const iso = '2026-11-02T00:33:00Z'
  const here = defaultTimeZone()
  const short = formatDueShort(iso, here)
  assert.doesNotMatch(short, /2026/)
  assert.match(short, /^\d\d\/\d\d \d\d:\d\d$/)
  const other = here === 'Asia/Taipei' ? 'UTC' : 'Asia/Taipei'
  assert.notEqual(formatDueShort(iso, other), formatDueShort(iso, here))
})
