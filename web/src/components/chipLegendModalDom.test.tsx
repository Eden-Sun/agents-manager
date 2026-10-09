/**
 * #995：色票與手機狀態卡是蓋整個畫面的對話框，必須被 `dialogOpen()` 認出來，上一頁（routeSync 派的 Escape）才會先關框，
 * 桌機的 ⌥↑／Ctrl+數字 也才會讓路。認的條件是 aria-modal 屬性，所以兩個元件的 role="dialog" 都要帶它。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { useStore } from '../store/store'
import { dialogOpen } from '../lib/dialogOpen'
import { ChipLegend } from './ChipLegend'
import { BotStatusCard, type ChipHints } from './BotStatusCard'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const hints: ChipHints = { current: false, unread: 0, needsReply: false, waitsKids: false, kidsRunning: 0, cacheTitle: null }

beforeEach(() => {
  fakeApi()
  useStore.setState({
    bots: [{
      id: 'b1', name: 'bot-b1', project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
      autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: true, primary_position: 0, cwd: null,
    }],
    runs: {}, botUnread: {}, connected: true, defaultConnected: true, socket: 'open',
  } as never)
})

/** 上一頁（routeSync.topDialog）做的事：對最上層的 aria-modal 對話框派一個冒泡的 Escape。 */
function popBack(): void {
  const top = document.querySelector<HTMLElement>('[aria-modal]')!
  top.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
}

test('色票開著時 dialogOpen() 為真，上一頁派的 Escape 會關掉它', async () => {
  let closed = 0
  await mount(<ChipLegend onClose={() => closed++} />)
  assert.equal(dialogOpen(), true)
  await act(async () => popBack())
  assert.equal(closed, 1)
})

test('手機狀態卡（anchor=null）也算對話框，上一頁會關掉它', async () => {
  let closed = 0
  await mount(<BotStatusCard botId="b1" hints={hints} anchor={null} onClose={() => closed++} onLegend={() => {}} />)
  assert.equal(dialogOpen(), true)
  await act(async () => popBack())
  assert.equal(closed, 1)
})
