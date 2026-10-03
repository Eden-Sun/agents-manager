/**
 * ⟳ 重啟徽章（RuntimeDriftBadge）：daemon 的 `bot.needs_restart` 是「要重啟」的唯一來源（#353），徽章的落差明細只列已知欄位
 * （model／effort／fast／identity）。人設、args、env 改了沒有明細——徽章也不能因此消失，不然只有設定面板開著時看得到要重啟。
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
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', model: 'opus', effort: null, fast: false, identity: null, live_apply_deferred: false, needs_restart: false } as unknown as Bot
const run = { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'idle', runtime_model: 'opus', runtime_effort: null, runtime_fast: null, runtime_identity: '' } as unknown as Run

test('設定改了、daemon 說要重啟，但改的是已知落差欄位以外的東西（人設…）：⟳ 徽章照樣出現', async () => {
  useStore.setState({ bots: [{ ...bot, needs_restart: true } as Bot], runs: { b1: run }, busy: {} })
  await mount(<RuntimeDriftBadge botId="b1" />)
  const button = document.querySelector<HTMLButtonElement>('button.drift-badge')
  assert.ok(button, 'needs_restart 為真就要有徽章，哪怕找不出是哪個欄位')
  assert.ok(button.textContent?.includes('需重啟'), button.textContent ?? '')
  await act(async () => {
    useStore.setState({ bots: [{ ...bot, needs_restart: false } as Bot] })
  })
  assert.equal(document.querySelector('button.drift-badge'), null, '沒有落差也不要求重啟：不顯示')
})
