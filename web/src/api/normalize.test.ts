import test from 'node:test'
import assert from 'node:assert/strict'
import { toKindQuota, toState } from './normalize.ts'

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

/**
 * **issue #492** 的三態。`pick` 的用途是「挑第一個有值的鍵」，所以它把 `null` 也收斂成 `undefined`——
 * 拿它判欄位在不在，daemon 說的「現在沒有批次在跑」就會被讀成「舊 daemon，不知道」，而那一格正是
 * 用來清掉卡住的重啟進度的。存在與否只能用 `in`。
 */
test('#492 restart_batch 的三態：在但空是 null（沒有批次），欄位不在才是 undefined（不知道）', () => {
  assert.equal(toState({ restart_batch: null }).restart_batch, null)
  assert.equal(toState({ restart_batch: '' }).restart_batch, null)
  assert.equal(toState({ restart_batch: 'b1' }).restart_batch, 'b1')
  assert.equal(toState({}).restart_batch, undefined)
})
