import test from 'node:test'
import assert from 'node:assert/strict'
import { contextLabel } from './contextLabel'

test('contextLabel：有百分比寫「40%（12k / 200k）」，視窗 1M 跟狀態列一樣寫 1M', () => {
  assert.equal(contextLabel({ context_used_pct: 40, context_used_tokens: 12_000, context_size: 200_000 }), '40%（12k / 200k）')
  assert.equal(contextLabel({ context_used_pct: 5, context_used_tokens: 50_000, context_size: 1_000_000 }), '5%（50k / 1M）')
  assert.equal(contextLabel({ context_used_pct: 0, context_used_tokens: 300, context_size: 200_000 }), '0%（300 / 200k）')
  assert.equal(contextLabel({ context_used_pct: 40, context_used_tokens: null, context_size: null }), '40%')
})

test('contextLabel：只有 token 數（agy）寫「約 N tokens」，用量 0 或沒有就不顯示', () => {
  assert.equal(contextLabel({ context_used_pct: null, context_used_tokens: 300, context_size: null }), '約 300 tokens')
  assert.equal(contextLabel({ context_used_pct: null, context_used_tokens: 12_400, context_size: null }), '約 12k tokens')
  assert.equal(contextLabel({ context_used_pct: null, context_used_tokens: 0, context_size: null }), null)
  assert.equal(contextLabel(null), null)
  assert.equal(contextLabel(undefined), null)
})
