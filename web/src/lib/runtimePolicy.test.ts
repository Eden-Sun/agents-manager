import test from 'node:test'
import assert from 'node:assert/strict'
import { cleanSettings, firstRangeError, formatContext, nearestTickIndex, sameSettings } from './runtimePolicy'

test('cleanSettings 去掉 null，只送有值的欄位', () => {
  assert.deepEqual(cleanSettings({ context_tokens: null, keep_warm: false, keep_warm_after_min: 30 }), { keep_warm: false, keep_warm_after_min: 30 })
  assert.deepEqual(cleanSettings({ keep_warm: null }), {})
})

test('nearestTickIndex 取最近的刻度；缺值＝第 0 格', () => {
  assert.equal(nearestTickIndex(null), 0)
  assert.equal(nearestTickIndex(512_000), 3)
  assert.equal(nearestTickIndex(300_000), 2)
  assert.equal(formatContext(1_000_000), '1M')
  assert.equal(formatContext(128_000), '128k')
})

test('firstRangeError：分鐘範圍與熱壓＞保溫', () => {
  assert.equal(firstRangeError({ keep_warm_after_min: 60 }), 'keep_warm_after_min')
  assert.equal(firstRangeError({ keep_warm_after_min: 30, hot_compact_after_min: 30 }), 'hot_compact_after_min')
  assert.equal(firstRangeError({ hot_compact_min_pct: 101 }), 'hot_compact_min_pct')
  assert.equal(firstRangeError({ keep_warm_after_min: 30, hot_compact_after_min: 50, hot_compact_min_pct: 60 }), null)
})

test('sameSettings 不看 null 與順序', () => {
  assert.equal(sameSettings({ keep_warm: true, context_tokens: null }, { keep_warm: true }), true)
  assert.equal(sameSettings({ keep_warm: true }, { keep_warm: false }), false)
})
