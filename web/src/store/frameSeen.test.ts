import test from 'node:test'
import assert from 'node:assert/strict'
import { gateFrame } from './frameSeen.ts'
import { restartProgress } from './restartBatch.ts'
import type { RestartBatch } from '../api/types.ts'

test('#521 同一條連線上重複的耐久幀要丟掉', () => {
  const first = gateFrame(0, 'bot_changed', 7)
  assert.deepEqual(first, { skip: false, seen: 7 })
  assert.deepEqual(gateFrame(first.seen, 'bot_changed', 7), { skip: true, seen: 7 })
  assert.deepEqual(gateFrame(first.seen, 'bot_changed', 8), { skip: false, seen: 8 })
})

test('#521 turn_progress 與 resync 照舊放行，也不推進水位', () => {
  assert.deepEqual(gateFrame(5, 'turn_progress', 9), { skip: false, seen: 5 })
  assert.deepEqual(gateFrame(5, 'resync', 9), { skip: false, seen: 5 })
  // progress 推過頭的話，之後真正的耐久幀會被自己的水位擋掉。
  assert.deepEqual(gateFrame(5, 'bot_changed', 6), { skip: false, seen: 6 })
})

test('#521 沒有 seq 的幀不判斷（舊 daemon／手造的幀）', () => {
  assert.deepEqual(gateFrame(5, 'bot_changed', undefined), { skip: false, seen: 5 })
})

/** daemon 重啟後 seq 從頭數起：水位是「每條連線」的，重連歸零才不會把新 daemon 的小號碼整段擋掉（#368）。 */
test('#521 新連線從 0 起算，號碼比上一顆 daemon 小的幀照收', () => {
  assert.deepEqual(gateFrame(0, 'bot_changed', 3), { skip: false, seen: 3 })
})

const batch = (over: Partial<RestartBatch> = {}): RestartBatch => ({
  id: 'b1',
  total: 3,
  done: 0,
  current: null,
  ok: [],
  failed: [],
  skipped: [],
  finished: false,
  ...over,
})

/** 重複的 `bots_restart_progress` 會讓進度多算一次、`ok` 多一筆——擋掉之後就不會。 */
test('#521 重複的一鍵重啟進度幀不會重複累加', () => {
  const frame = { batch_id: 'b1', name: 'alfa', status: 'ok', total: 3 }
  let seen = 0
  let b = batch()
  for (const seq of [11, 11]) {
    const gate = gateFrame(seen, 'bots_restart_progress', seq)
    if (gate.skip) continue
    seen = gate.seen
    b = restartProgress(b, frame)
  }
  assert.equal(b.done, 1, '同一個 seq 送兩次只能算一次')
  assert.deepEqual(b.ok, ['alfa'])
  // 真的又完成一顆（新的 seq）才加。
  const gate = gateFrame(seen, 'bots_restart_progress', 12)
  assert.equal(gate.skip, false)
  b = restartProgress(b, { ...frame, name: 'bravo' })
  assert.equal(b.done, 2)
  assert.deepEqual(b.ok, ['alfa', 'bravo'])
})
