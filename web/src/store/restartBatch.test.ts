import test from 'node:test'
import assert from 'node:assert/strict'
import { joinRunningBatch, reconcileBatch, restartProgress } from './restartBatch.ts'
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

/**
 * **issue #492**：`bots_restart_done` 是唯一會把進度收尾的來源，而它有兩條收不到的路——
 * 批次跑到一半 daemon 重啟（那一則永遠不會送，而且 daemon 回來之後也沒有那一批了），
 * 以及客戶端落到全量 resync（走 `refreshState`、backlog 整段不重播）。兩條都會停在「重啟中 k/N」，
 * 而那顆晶片會一直蓋著一鍵重啟的觸發鈕。所以 `refreshState` 要拿快照對帳。
 */
test('#492 批次中途 daemon 重啟：快照說沒有批次在跑，手上那份過期的要清掉', () => {
  const running = batch({ id: 'b1', total: 5, done: 2, current: 'C' })
  assert.equal(reconcileBatch(running, null), null)
})

test('#492 落到全量 resync：跑的是另一批（別人按的）時，手上那份一樣清掉', () => {
  const running = batch({ id: 'b1', done: 2 })
  assert.equal(reconcileBatch(running, 'b2'), null)
})

test('#492 同一批還在跑：原樣留著（連物件都不換，免得白重繪）', () => {
  const running = batch({ id: 'b1', done: 2 })
  assert.equal(reconcileBatch(running, 'b1'), running)
})

test('#492 已經跑完的摘要不清：那是給人看的，要由使用者自己收起來', () => {
  const done = batch({ id: 'b1', finished: true, done: 2, ok: ['A', 'B'] })
  assert.equal(reconcileBatch(done, null), done)
})

/** 舊 daemon 沒有 `restart_batch` 欄位：`undefined`＝不知道。把「不知道」當成「沒有在跑」會在真的有批次時清掉進度。 */
test('#492 舊 daemon（欄位不存在）：不知道就不動手上的進度', () => {
  const running = batch({ id: 'b1', done: 2 })
  assert.equal(reconcileBatch(running, undefined), running)
})

test('#492 手上本來就沒有批次：怎麼問都還是沒有', () => {
  assert.equal(reconcileBatch(null, 'b1'), null)
  assert.equal(reconcileBatch(null, null), null)
})
