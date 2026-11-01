import test from 'node:test'
import assert from 'node:assert/strict'
import { apiBotSpendUsd, toApiBotUsage } from './apiBotUsage'

test('累計花費＝(recorded+reserved+unknown)/1e6；預留與不明先當花掉算', () => {
  const u = toApiBotUsage({
    attempts: { total: 4, recorded: 1, reserved: 1, unknown: 1, failed: 1 },
    cost_microusd: { recorded: 2000, reserved: 4_100_000, unknown: 4_200_000 },
    tokens: { input: 100, output: 50, cache_read: 300, cache_creation: 100 },
    cache_hit_rate: 0.6,
    last: { speed: 'fast', at: '2026-11-01T00:00:00Z', input_tokens: 100, output_tokens: 50 },
  })
  assert.equal(apiBotSpendUsd(u), 8.302)
  assert.equal(u.cache_hit_rate, 0.6)
  assert.equal(u.last?.speed, 'fast')
})

test('空回應與缺欄位不炸：全部歸零、命中率 null、last null', () => {
  const u = toApiBotUsage({})
  assert.equal(apiBotSpendUsd(u), 0)
  assert.equal(u.cache_hit_rate, null)
  assert.equal(u.last, null)
  assert.equal(u.attempts.total, 0)
})
