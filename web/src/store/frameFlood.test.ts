/**
 * 多顆 bot 同時 working 時的 WS 幀洪水：`turn_progress` 一個 run 每秒約 4 幀。
 * 每個 store 更新都會通知所有掛著的 selector（長對話開著時有幾百個），所以「每一幀各多一次 set」就是白白的 N 倍成本。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'

const { useStore, dispatchFrameForTest } = await import('./store.ts')
const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })

test('同一顆 bot 的一串 turn_progress：通知次數只跟節流後的 liveReply 更新有關，不是每幀一次', () => {
  reset()
  routeDaemon(() => json({}))
  useStore.setState({ lastSeq: 0, lastDurableSeq: 0 })
  let notified = 0
  const off = useStore.subscribe(() => { notified++ })
  try {
    for (let i = 1; i <= 40; i++) {
      dispatchFrameForTest({ seq: i, type: 'turn_progress', data: { bot_id: 'b1', turn_id: 't1', text: `進度 ${i}`, revision: i } })
    }
  } finally {
    off()
  }
  assert.ok(notified <= 2, `40 幀 progress 通知了 ${notified} 次（頭一幀立刻套一次，其餘合併到下一拍）`)
  assert.equal(useStore.getState().liveReply.b1?.revision, 1, '頭一幀立刻套')
})

test('progress 不再各自 set，但看過的 seq 一點都沒丟：下一個耐久幀之後 lastSeq 跟上', () => {
  reset()
  routeDaemon(() => json({}))
  useStore.setState({ lastSeq: 0, lastDurableSeq: 0 })
  dispatchFrameForTest({ seq: 5, type: 'turn_progress', data: { bot_id: 'b1', turn_id: 't1', text: 'x', revision: 1 } })
  dispatchFrameForTest({ seq: 6, type: 'turn_progress', data: { bot_id: 'b1', turn_id: 't1', text: 'y', revision: 2 } })
  dispatchFrameForTest({ seq: 7, type: 'supervisor_changed' })
  assert.equal(useStore.getState().lastSeq, 7)
  assert.equal(useStore.getState().lastDurableSeq, 7)
  dispatchFrameForTest({ seq: 9, type: 'turn_progress', data: { bot_id: 'b1', turn_id: 't1', text: 'z', revision: 3 } })
  dispatchFrameForTest({ seq: 10, type: 'supervisor_changed' })
  assert.equal(useStore.getState().lastSeq, 10)
  assert.equal(useStore.getState().lastDurableSeq, 10, '耐久 seq 只跟著耐久幀走')
})
