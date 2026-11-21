import test from 'node:test'
import assert from 'node:assert/strict'
import { seed, runTurn, status, turn } from './unreadTurn.harness.ts'
import { dispatchFrameForTest, useStore } from './store.ts'

test('一般回合照記未讀（對照組）', () => {
  seed()
  runTurn('t1', 'web-1', false)
  assert.equal(useStore.getState().botUnread.b1, 1)
})

test('保溫回覆（DB 舊前綴 keepalive:）也不亮；訊息沒帶 keep_warm 時靠回合前綴擋住', () => {
  seed()
  runTurn('t1', 'keepalive:anchor0', false)
  assert.equal(useStore.getState().botUnread.b1, undefined)
})

test('保溫之後接一個真的回合：只有真的那個記一筆', () => {
  seed()
  runTurn('t1', 'keep-warm:anchor1', true)
  runTurn('t2', 'web-2', false)
  assert.equal(useStore.getState().botUnread.b1, 1)
})

test('終端的 working→idle 邊緣補記：保溫回合在飛時也不記', () => {
  seed()
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn('t1', 'in_flight', 'keep-warm:anchor1') } })
  dispatchFrameForTest(status('working'))
  dispatchFrameForTest(status('idle'))
  assert.equal(useStore.getState().botUnread.b1, undefined)
})

test('run 帶 keep_warm_replied_at／keep_warm_skip：進 store；送新 prompt 後 daemon 清成 null 也跟著清', () => {
  seed()
  dispatchFrameForTest(status('idle', { keep_warm_replied_at: '2026-10-05T10:00:30Z', keep_warm_skip: true, cache_kept_warm_at: '2026-10-05T10:00:30Z' }))
  const r = useStore.getState().runs.b1!
  assert.equal(r.keep_warm_replied_at, '2026-10-05T10:00:30Z')
  assert.equal(r.keep_warm_skip, true)
  assert.equal(r.cache_kept_warm_at, '2026-10-05T10:00:30Z')
  dispatchFrameForTest(status('idle', { keep_warm_replied_at: null, keep_warm_skip: false }))
  const after = useStore.getState().runs.b1!
  assert.equal(after.keep_warm_replied_at, null)
  assert.equal(after.keep_warm_skip, false)
})
