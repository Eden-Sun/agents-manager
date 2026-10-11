/**
 * #1240：第一次載入對話失敗後，畫面要給「重試」，不能一直停在「載入訊息中…」；按重試成功後訊息出現、按鈕消失。
 * 真的掛 `ChatPanel`，後端是共用的 mock，只有這一支 GET 訊息的請求在第一次時失敗。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest() // 要在拆 DOM 之前
  await teardownDom()
})

const retryButton = () => [...document.querySelectorAll('button')].find((b) => b.textContent === '重試') ?? null

test('第一次載入對話失敗：畫出重試鈕、不再轉圈；重試成功後鈕消失', { timeout: 30_000 }, async () => {
  resetStoreForTest()
  mockApi(sharedMock)
  let fail = true
  const real = sharedMock.request.bind(sharedMock)
  sharedMock.request = (method, path, body) => {
    if (fail && method === 'GET' && /\/bots\/[^/]+\/messages/.test(path)) {
      return Promise.reject(Object.assign(new Error('boom'), { status: 500, body: { error: 'boom' } }))
    }
    return real(method, path, body)
  }
  try {
    await useStore.getState().refreshState()
    const bot = useStore.getState().bots.find((b) => b.name === 'am-claude-kid')!
    assert.ok(bot, 'mock 要有 am-claude-kid')
    useStore.getState().selectBot(bot.id)
    await mount(<ChatPanel onOpenSidebar={() => {}} />)
    await until(() => retryButton() !== null, '重試鈕出現')
    assert.doesNotMatch(document.body.textContent ?? '', /載入訊息中/, '失敗之後不該再轉圈')

    fail = false
    click(retryButton()!)
    await until(() => useStore.getState().loadedBots[bot.id] === true, '重試成功')
    await settle(100)
    assert.equal(retryButton(), null, '重試成功後鈕要消失')
  } finally {
    sharedMock.request = real
  }
})
