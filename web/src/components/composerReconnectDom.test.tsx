import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeWebSocket, mockApi, mount, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { installManualTimers } from '../testing/manualTimers'
import { untilVirtual } from '../testing/untilVirtual'
import { resetStoreForTest, useStore } from '../store/store'
import { ChatPanel } from './ChatPanel'

virtualMockTime()
let timers: ReturnType<typeof installManualTimers>
const until = (cond: () => boolean | Promise<boolean>, what: string) => untilVirtual(timers.clock, cond, what, { alsoAdvance: advanceMockTime })
const elapse = (ms: number) =>
  act(async () => {
    await timers.clock.advance(ms)
    await advanceMockTime(ms)
  })
beforeEach(() => {
  timers = installManualTimers()
})
afterEach(async () => {
  timers.restore()
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const textarea = () => document.querySelector<HTMLTextAreaElement>('.composer textarea')!

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
  await elapse(200)
  return { mock, requests, sockets, bot }
}

async function dropAndReconnect(sockets: ReturnType<typeof fakeWebSocket>) {
  const before = sockets.connects()
  await sockets.drop()
  await until(() => sockets.connects() === before + 1, '自動重連')
  await sockets.open()
}

test('#760 還沒送進 daemon 的字：掉線重連後輸入框還在、而且之後補送', { timeout: 60_000 }, async () => {
  const { mock, sockets, bot } = await boot()
  await typeInto(textarea(), 'half typed message')
  await dropAndReconnect(sockets)
  await elapse(300)
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
    await elapse(700)
    assert.equal(textarea().value, 'typed while offline')
    offline = false
    await dropAndReconnect(sockets)
    await elapse(300)
    assert.equal(textarea().value, 'typed while offline', '重連後輸入框還在')
    await until(async () => {
      const r = (await mock.request('GET', '/drafts')) as { drafts: { key: string; text: string }[] }
      return r.drafts.some((d) => d.key === `bot:${bot.id}` && d.text === 'typed while offline')
    }, '重連之後補送進 daemon')
  } finally {
    globalThis.fetch = realFetch
  }
})
