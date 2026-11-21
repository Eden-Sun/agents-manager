import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll, fakeWebSocket } from '../testing/domHarness'
import { advanceMockTime, sharedMock, virtualMockTime } from '../testing/sharedMock'
import { installManualTimers } from '../testing/manualTimers'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
const opened: string[] = []
let timers: ReturnType<typeof installManualTimers>
const elapse = (ms: number) =>
  act(async () => {
    await timers.clock.advance(ms)
    await advanceMockTime(ms)
  })
afterEach(async () => {
  timers.restore()
  await unmountAll()
  for (const pane of opened.splice(0)) await mock.request('DELETE', `/hosts/local/shells/${encodeURIComponent(pane)}?confirm=true`).catch(() => {})
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})
beforeEach(() => {
  fakeWebSocket()
  timers = installManualTimers()
  localStorage.removeItem('am.shellKeySyncOff')
  localStorage.removeItem('am.shellKeySyncOn')
})

const mock = sharedMock

function asPhone(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

async function openShell() {
  mockApi(mock)
  await useStore.getState().refreshState()
  const shell = (await mock.request('POST', '/hosts/local/shells', {})) as { pane_id: string; cwd: string }
  opened.push(shell.pane_id)
  await mount(<HostShellPanel host="local" paneId={shell.pane_id} cwd={shell.cwd} embedded />)
  await elapse(300)
  return shell
}

const cmdInput = () => document.querySelector<HTMLInputElement>('.shell-cmd')!
const sendErrBanner = () => document.querySelector<HTMLElement>('[role="alert"].shell-err')

async function type(v: string) {
  const el = cmdInput()
  await act(async () => {
    const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!
    set.call(el, v)
    el.dispatchEvent(new Event('input', { bubbles: true }))
  })
}
const enter = () => cmdInput().dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }))

test('送指令失敗：錯誤留在畫面上，不被下一次輪詢清掉，也不叫「讀取失敗」', async () => {
  const restore = asPhone()
  const fetchBefore = globalThis.fetch
  try {
    const shell = await openShell()
    const textPath = `/shells/${encodeURIComponent(shell.pane_id)}/text`
    const fetchNow = globalThis.fetch
    globalThis.fetch = (async (url: string, init?: { method?: string; body?: string }) => {
      if (init?.method === 'POST' && String(url).endsWith(textPath)) {
        return new Response(JSON.stringify({ error: 'upstream', message: 'boom' }), { status: 502 })
      }
      return fetchNow(url, init as RequestInit)
    }) as unknown as typeof fetch

    await type('ls')
    await act(async () => enter())
    await elapse(2500)
    const banner = sendErrBanner()
    assert.ok(banner, '送出失敗要有 role=alert 的 .shell-err')
    assert.match(banner.textContent ?? '', /送出失敗/)
    assert.doesNotMatch(document.body.textContent ?? '', /最後一次讀取失敗/, '送出失敗不是讀取失敗')
    assert.equal(cmdInput().value, 'ls', '沒送進去，指令留在輸入框')
  } finally {
    globalThis.fetch = fetchBefore
    restore()
  }
})

test('之後送成功就收掉送出失敗的說明', async () => {
  const restore = asPhone()
  const fetchBefore = globalThis.fetch
  try {
    const shell = await openShell()
    const textPath = `/shells/${encodeURIComponent(shell.pane_id)}/text`
    const fetchNow = globalThis.fetch
    let fail = true
    globalThis.fetch = (async (url: string, init?: { method?: string; body?: string }) => {
      if (fail && init?.method === 'POST' && String(url).endsWith(textPath)) {
        return new Response(JSON.stringify({ error: 'upstream', message: 'boom' }), { status: 502 })
      }
      return fetchNow(url, init as RequestInit)
    }) as unknown as typeof fetch

    await type('ls')
    await act(async () => enter())
    await elapse(300)
    assert.ok(sendErrBanner(), '先失敗一次')

    fail = false
    await act(async () => enter())
    await elapse(300)
    assert.equal(sendErrBanner(), null, '送成功後說明收掉')
  } finally {
    globalThis.fetch = fetchBefore
    restore()
  }
})
