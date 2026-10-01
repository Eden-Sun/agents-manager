/**
 * #760 WS 心跳斷線、重連：輸入框裡打到一半的字不能丟。重連（`onopen`）會整份重抓 state／訊息／草稿，
 * 草稿以 daemon 為準，但本機還沒送進 daemon 的修改（debounce 還沒到、或 PUT 在斷線期間失敗）一定要留著、之後補送。
 * 這裡的 WebSocket 是假的，`HttpTransport.openSocket` 的重連與 backoff 照真的跑。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeWebSocket, mockApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { sharedMock } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest() // 要在拆 DOM 之前：關 socket 會拿掉 window 上的監聽
  teardownDom()
})

const textarea = () => document.querySelector<HTMLTextAreaElement>('.composer textarea')!

async function until(cond: () => boolean | Promise<boolean>, what: string, ms = 8000): Promise<void> {
  const end = Date.now() + ms
  while (Date.now() < end) {
    if (await cond()) return
    await settle(50)
  }
  assert.fail(`等不到：${what}`)
}

async function boot() {
  const mock = sharedMock
  const requests = mockApi(mock)
  const sockets = fakeWebSocket()
  await useStore.getState().bootstrap()
  assert.equal(useStore.getState().bootError, null)
  await sockets.open()
  const bot = useStore.getState().bots.find((b) => b.name === 'am-claude')!
  useStore.getState().selectBot(bot.id)
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  await settle(200)
  return { mock, requests, sockets, bot }
}

/** 掉線 → 等 `openSocket` 自己排的重連（backoff ≤ 約 0.4 秒）→ 伺服器接受。 */
async function dropAndReconnect(sockets: ReturnType<typeof fakeWebSocket>) {
  const before = sockets.connects()
  await sockets.drop()
  await until(() => sockets.connects() === before + 1, '自動重連')
  await sockets.open()
}

test('#760 還沒送進 daemon 的字：掉線重連後輸入框還在、而且之後補送', { timeout: 60_000 }, async () => {
  const { mock, sockets, bot } = await boot()
  await typeInto(textarea(), 'half typed message')
  // 不等 debounce（400ms）就斷線重連：重連時 `draftSync.load()` 拉到的 daemon 還沒有這份草稿。
  await dropAndReconnect(sockets)
  await settle(300)
  assert.equal(textarea().value, 'half typed message', '重連之後輸入框不能被清掉')
  assert.equal(useStore.getState().drafts[`bot:${bot.id}`], 'half typed message')
  await until(async () => {
    const r = (await mock.request('GET', '/drafts')) as { drafts: { key: string; text: string }[] }
    return r.drafts.some((d) => d.key === `bot:${bot.id}` && d.text === 'half typed message')
  }, '字最後補送進 daemon')
})

test('#760 斷線期間 PUT 失敗：重連後字還在，重連時馬上補送', { timeout: 60_000 }, async () => {
  const { mock, sockets, bot } = await boot()
  const realFetch = globalThis.fetch
  let offline = true
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    if (offline && init?.method === 'PUT') throw new TypeError('Failed to fetch')
    return (realFetch as (a: string, b?: unknown) => Promise<Response>)(input, init)
  }) as unknown as typeof fetch
  try {
    await typeInto(textarea(), 'typed while offline')
    await sockets.drop()
    await settle(700) // debounce 已到、PUT 失敗、dirty 留著等重試
    assert.equal(textarea().value, 'typed while offline')
    offline = false
    await dropAndReconnect(sockets)
    await settle(300)
    assert.equal(textarea().value, 'typed while offline', '重連後輸入框還在')
    await until(async () => {
      const r = (await mock.request('GET', '/drafts')) as { drafts: { key: string; text: string }[] }
      return r.drafts.some((d) => d.key === `bot:${bot.id}` && d.text === 'typed while offline')
    }, '重連之後補送進 daemon')
  } finally {
    globalThis.fetch = realFetch
  }
})
