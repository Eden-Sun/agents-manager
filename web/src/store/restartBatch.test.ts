import test from 'node:test'
import assert from 'node:assert/strict'
import { joinRunningBatch, restartProgress } from './restartBatch.ts'
import type { RestartBatch } from '../api/types.ts'

const batch = (over: Partial<RestartBatch> = {}): RestartBatch => ({
  id: 'b1', total: 2, done: 0, current: null, ok: [], failed: [], skipped: [], finished: false, ...over,
})

/** daemon 902a997：輪到那一顆時狀態變了就跳過，進度要算它處理過了，不然永遠停在 1/2。 */
test('輪到時狀態變了（skipped）：算處理過、列進跳過清單，理由用 daemon 寫好的', () => {
  const next = restartProgress(batch({ current: 'x' }), {
    batch_id: 'b1', total: 2, bot_id: 'bot-a', name: 'A', status: 'skipped', reason: 'no_longer_pending', reason_label: '開始回合了',
  })
  assert.equal(next.done, 1)
  assert.equal(next.current, null)
  assert.deepEqual(next.skipped, [{ bot_id: 'bot-a', name: 'A', reason: 'no_longer_pending', reason_label: '開始回合了' }])
})

test('別批的事件不動；加入的那一批總數從事件補上', () => {
  const b = batch()
  assert.equal(restartProgress(b, { batch_id: 'other', status: 'ok', name: 'A' }), b)
  const joined = joinRunningBatch(null, 'b9')
  assert.equal(joined.total, 0)
  const next = restartProgress(joined, { batch_id: 'b9', total: 3, name: 'A', status: 'restarting' })
  assert.equal(next.total, 3)
  assert.equal(next.current, 'A')
})

test('已經在看同一批時再按一次：不歸零', () => {
  const running = batch({ id: 'b1', done: 1, ok: ['A'] })
  assert.equal(joinRunningBatch(running, 'b1'), running)
})
