/**
 * 多顆 bot 同時 working 時的 WS 幀洪水：`turn_progress` 一個 run 每秒約 4 幀。
 * 每個 store 更新都會通知所有掛著的 selector（長對話開著時有幾百個），所以「每一幀各多一次 set」就是白白的 N 倍成本。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import { installManualTimers } from '../testing/manualTimers.ts'

const { useStore, dispatchFrameForTest } = await import('./store.ts')
const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 })

test('assistant message 到達後，已排程的 turn_progress 尾端不能把 partial reply 寫回來', async () => {
  const timers = installManualTimers()
  try {
    reset()
    routeDaemon(() => json({}))
    dispatchFrameForTest({
      type: 'turn_updated',
      data: { bot_id: 'b1', turn: { id: 't1', conversation_id: 'c', run_id: 'r1', origin: 'web', status: 'in_flight', delivery: 'ok', created_at: '2026-10-03T00:00:00Z' } },
    })
    dispatchFrameForTest({ type: 'turn_progress', data: { bot_id: 'b1', turn_id: 't1', text: 'partial 1', revision: 1 } })
    dispatchFrameForTest({ type: 'turn_progress', data: { bot_id: 'b1', turn_id: 't1', text: 'partial 2', revision: 2 } })
    assert.equal(timers.clock.pending, 1)
    dispatchFrameForTest({
      type: 'message_added',
      data: { bot_id: 'b1', message: { id: 'm1', conversation_id: 'c', turn_id: 't1', role: 'assistant', content: 'final', source: 'hook', created_at: '2026-10-03T00:00:01Z' } },
    })
    assert.equal(useStore.getState().liveReply.b1, undefined, 'final message clears the partial')
    await timers.clock.advance(250)
    assert.equal(useStore.getState().liveReply.b1, undefined, 'the delayed tail must not resurrect it')
  } finally {
    await timers.clock.advance(250)
    timers.restore()
  }
})

test('resetStoreForTest cancels each bot trailing-progress timer before a fresh store can receive it', async () => {
  const timers = installManualTimers()
  try {
    reset()
    dispatchFrameForTest({ type: 'turn_progress', data: { bot_id: 'b-stale', turn_id: 't-old', text: 'first', revision: 1 } })
    dispatchFrameForTest({ type: 'turn_progress', data: { bot_id: 'b-stale', turn_id: 't-old', text: 'trailing', revision: 2 } })
    assert.equal(timers.clock.pending, 1)
    reset()
    assert.equal(timers.clock.pending, 0, 'reset drops the scheduled callback and its retained closure')
    await timers.clock.advance(250)
    assert.equal(useStore.getState().liveReply['b-stale'], undefined)
  } finally {
    await timers.clock.advance(250)
    timers.restore()
  }
})

test('一串 turn_progress 只通知頭幀與合併尾幀，最後保留最新 revision', async () => {
  const timers = installManualTimers()
  let off: (() => void) | null = null
  try {
    reset()
    routeDaemon(() => json({}))
    useStore.setState({ lastSeq: 0, lastDurableSeq: 0 })
    let notified = 0
    off = useStore.subscribe(() => { notified++ })
    for (let i = 1; i <= 40; i++) {
      dispatchFrameForTest({ seq: i, type: 'turn_progress', data: { bot_id: 'b1', turn_id: 't1', text: `進度 ${i}`, revision: i } })
    }
    assert.equal(notified, 1, '頭幀立即通知一次，其餘 39 幀排進同一個 timer')
    assert.equal(useStore.getState().liveReply.b1?.revision, 1, '頭一幀先顯示')
    await timers.clock.advance(250)
    assert.equal(notified, 2, '250 ms 尾幀再通知一次，不會每幀喚醒長對話 selector')
    assert.equal(useStore.getState().liveReply.b1?.revision, 40, '合併尾幀帶最新內容')
    off()
    off = null
  } finally {
    off?.()
    await timers.clock.advance(250)
    timers.restore()
  }
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
