/**
 * 單顆 bot 的重啟鈕（#重啟連點）：⟳ 徽章、設定面板的「立即重啟」、更新徽章是三個各自有本地 `restarting` 狀態的按鈕，
 * 同一顆 bot 只要有任何一處在重啟（store 的 `busy['restart:<id>']`），所有按鈕都要 disable——不然使用者換一個鈕再按，
 * store 的 `guarded` 會悄悄吞掉第二下（回 false、什麼都不說），畫面卻像沒反應。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import type { Bot, Run } from '../api/types'
import { RuntimeDriftBadge } from './RuntimeDriftBadge'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(() => {
  resetStoreForTest()
  teardownDom()
})

const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', model: 'opus', effort: null, fast: false, identity: null, live_apply_deferred: false } as unknown as Bot
const run = { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle', runtime_model: 'sonnet', runtime_effort: null, runtime_fast: null, runtime_identity: '' } as unknown as Run

test('別處已經在重啟這顆 bot：⟳ 徽章也 disable（不是只看自己的本地狀態）', async () => {
  useStore.setState({ bots: [bot], runs: { b1: run }, busy: {} })
  await mount(<RuntimeDriftBadge botId="b1" />)
  const button = () => document.querySelector<HTMLButtonElement>('button.drift-badge')!
  assert.ok(button(), '前提：有設定落差就有徽章')
  assert.equal(button().disabled, false)
  await act(async () => {
    useStore.setState({ busy: { 'restart:b1': true } })
  })
  assert.equal(button().disabled, true, '重啟進行中不能再按')
  await act(async () => {
    useStore.setState({ busy: {} })
  })
  assert.equal(button().disabled, false, '結束就放開')
})
