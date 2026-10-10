import test from 'node:test'
import assert from 'node:assert/strict'
import { reset } from './storeEnv.harness.ts'
import { dispatchFrameForTest, useStore } from './store.ts'

// #1098：重產連結不改 `share_enabled`，所以 `bot_share_changed` 要記一個數字，讓分享鈕與設定面板知道連結換了。

test('bot_share_changed 每來一次，那顆 bot 的 shareRev 加一', () => {
  reset()
  dispatchFrameForTest({ type: 'bot_share_changed', data: { bot_id: 'b1', enabled: true } })
  dispatchFrameForTest({ type: 'bot_share_changed', data: { bot_id: 'b1', enabled: true } })
  const rev = useStore.getState().shareRev
  assert.equal(rev.b1, 2)
  assert.equal(rev.b2, undefined)
})

test('沒有 bot_id 的 bot_share_changed 不丟例外、shareRev 不變', () => {
  reset()
  assert.doesNotThrow(() => dispatchFrameForTest({ type: 'bot_share_changed', data: { enabled: true } }))
  assert.deepEqual(useStore.getState().shareRev, {})
})
