/**
 * 重連之後該重抓的東西都要重抓（漏掉的 WS 事件沒有別的來源會補）：state（bot／run／turn／queued／未讀／主機）、額度、
 * 草稿、身分停用、已載入的對話，還有側欄的 pane 清單（只靠 `panes_changed` 事件＋30 秒輪詢，輪詢之間漏了事件就會舊最多 30 秒）。
 * 假 WebSocket、mock daemon，`openSocket` 的重連照真的跑。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeWebSocket, mockApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

test('重連（不是第一次連上）：state、額度、草稿、身分停用、已載入對話、pane 清單都重抓', { timeout: 60_000 }, async () => {
  const requests = mockApi(sharedMock)
  const sockets = fakeWebSocket()
  await useStore.getState().bootstrap()
  await sockets.open()
  const bot = useStore.getState().bots.find((b) => b.name === 'am-claude')!
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle(200)

  const mark = requests.length
  const before = sockets.connects()
  await sockets.drop()
  await until(() => sockets.connects() === before + 1, '自動重連')
  await sockets.open()
  await settle(300)

  const gets = requests.slice(mark).filter((r) => r.method === 'GET').map((r) => r.path.split('?')[0])
  for (const want of ['/api/state', '/api/quota', '/api/drafts', `/api/bots/${bot.id}/messages`, '/api/panes']) {
    assert.ok(gets.includes(want), `重連後沒有重抓 ${want}（抓了：${[...new Set(gets)].join(', ')}）`)
  }
  assert.ok(gets.some((p) => p.includes('identity')), `重連後沒有重抓身分停用（抓了：${[...new Set(gets)].join(', ')}）`)
})
