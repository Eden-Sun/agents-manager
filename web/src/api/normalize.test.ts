import test from 'node:test'
import assert from 'node:assert/strict'
import { toKindQuota } from './normalize.ts'

const reading = (stale?: boolean) => toKindQuota({
  five_hour: { used_pct: 41, resets_at: '2099-01-01T00:00:00Z', low: false, critical: false },
  seven_day: null,
  fable: null,
  reset_credits: null,
  limit_hit: null,
  plan: 'test',
  updated_at: '2026-09-22T10:00:00Z',
  ...(stale === undefined ? {} : { stale }),
  host: 'local',
}, 'claude')

test('額度快取回填的 stale 會保留給 UI', () => {
  assert.equal(reading(true)?.stale, true)
  assert.equal(reading(false)?.stale, false)
  // 舊 daemon 的 payload 沒有欄位時，維持 fresh 的相容預設。
  assert.equal(reading()?.stale, false)
})
