/**
 * 手機晶片列（單行橫捲）的 ◂ ▸：內容變多、開始溢出時，右邊的箭頭要出現。
 * `useHorizontalScroll` 的邊緣狀態只在 scroll／bar 尺寸改變時重算；晶片增加只會改 scrollWidth，不能靠它。
 * happy-dom 不排版：`.unread-bar` 的 scrollWidth 照晶片數量算，clientWidth 固定 300。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { UnreadChip } from './UnreadChip'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

/** 窄於 720px：matchMedia 對 `max-width: 720px` 回 true，其餘回 false。 */
function narrowWindow(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 720px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

const project = (id: string) => ({ id, label: `proj-${id}`, path: `/${id}`, host: 'local' })
const bot = (id: string) => ({
  id, name: `bot-${id}`, project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
})
const run = (botId: string) =>
  ({ id: `run-${botId}`, bot_id: botId, state: 'running', agent_status: 'working', workspace_id: null, pane_id: null, started_at: new Date().toISOString() }) as never

function setBots(ids: string[]) {
  useStore.setState({
    projects: [project('p1')],
    bots: ids.map(bot),
    runs: Object.fromEntries(ids.map((id) => [id, run(id)])),
    hosts: [], botOrder: {}, projectOrder: [], botUnread: {}, hiddenBotIds: [], connected: true, defaultConnected: true, socket: 'open',
    selectedBotId: null, selectedProjectId: null, supervisorProjectId: null, sidePanes: {}, unownedPanes: [],
  } as never)
}

/** 排版：每顆晶片 100px，列寬 300px（happy-dom 量不到，這裡自己給）。 */
function stubBar(): HTMLElement {
  const bar = document.querySelector<HTMLElement>('.unread-bar')!
  Object.defineProperty(bar, 'scrollWidth', { configurable: true, get: () => bar.querySelectorAll('.unread-chip').length * 100 })
  Object.defineProperty(bar, 'clientWidth', { configurable: true, get: () => 300 })
  return bar
}

beforeEach(() => {
  fakeApi()
})

test('手機單行橫捲：內容從沒溢出變成溢出（晶片增加）時，右邊的 ▸ 要出現，不必等使用者捲動', async () => {
  const restore = narrowWindow()
  try {
    setBots(['b1', 'b2'])
    await mount(<UnreadChip />)
    await settle()
    stubBar()
    await act(async () => {
      document.querySelector<HTMLElement>('.unread-bar')!.dispatchEvent(new Event('scroll'))
    })
    assert.equal(document.querySelector('.unread-bar-arrow.right'), null, '兩顆塞得下，沒有 ▸')

    setBots(['b1', 'b2', 'b3', 'b4', 'b5'])
    await settle()
    assert.ok(document.querySelectorAll('.unread-chip').length >= 5, '五顆都畫出來')
    assert.ok(document.querySelector('.unread-bar-arrow.right'), '五顆（500px）超過 300px，右邊要有 ▸')
  } finally {
    restore()
  }
})
