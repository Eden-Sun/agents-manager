/**
 * resume／壓縮後的讀數失效：daemon 把 `status_json` 遮成 `used_percentage: null`、並標 `_agm_context_unknown`。
 * 網頁要照 null 顯示「未知」（`context_unknown`），不能把 0 當成 0%；真讀數照常。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { toStatusInfo } from './normalize.ts'
import { contextLabel } from '../lib/contextLabel.ts'

test('遮罩後的讀數（null＋_agm_context_unknown）＝未知，不是 0%', () => {
  const s = toStatusInfo(JSON.stringify({
    _agm_context_unknown: true,
    context_window: { used_percentage: null, total_input_tokens: null, context_window_size: 1_000_000 },
  }))
  assert.ok(s)
  assert.equal(s.context_used_pct, null)
  assert.equal(s.context_used_tokens, null)
  assert.equal(s.context_unknown, true)
  assert.equal(contextLabel(s), null)
})

test('真讀數（0 以上）照常顯示，不標未知；舊 daemon 沒有旗標也是 false', () => {
  const real = toStatusInfo(JSON.stringify({
    context_window: { used_percentage: 15, total_input_tokens: 150_000, context_window_size: 1_000_000 },
  }))
  assert.ok(real)
  assert.equal(real.context_unknown, false)
  assert.equal(contextLabel(real), '15%（150k / 1M）')
  const rounded = toStatusInfo(JSON.stringify({ context_window: { used_percentage: 0, total_input_tokens: 300 } }))
  assert.ok(rounded)
  assert.equal(rounded.context_unknown, false)
  assert.equal(contextLabel(rounded), '0%')
})
