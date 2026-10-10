/**
 * #1127：送字／送鍵失敗的說明要留在畫面上，不被下一拍輪詢清掉，也不能被寫成「最後一次讀取失敗」。
 * 真的掛 `HostShellPanel` 進 happy-dom，後端是 `MockTransport`；送出的 POST 由測試包一層強制回 502。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { HostShellPanel } from './HostShellPanel'

virtualMockTime()
// 共用的 mock 一個主機最多 8 個 shell，而且跨測試檔累積：自己開的自己關。
const opened: string[] = []
afterEach(async () => {
  await unmountAll()
  for (const pane of opened.splice(0)) await mock.request('DELETE', `/hosts/local/shells/${encodeURIComponent(pane)}?confirm=true`).catch(() => {})
})
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})
beforeEach(() => {
  localStorage.removeItem('am.shellKeySyncOff')
  localStorage.removeItem('am.shellKeySyncOn')
})

const mock = sharedMock

/** 手機寬度：`matchMedia` 對 640px 斷點回 true，輸入列才會出現。 */
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
  await settle(300)
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
    // 至少兩輪 1 秒輪詢：成功的快照不能把送出錯誤清掉。
    await settle(2500)
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
    await settle(300)
    assert.ok(sendErrBanner(), '先失敗一次')

    fail = false
    await act(async () => enter())
    await settle(300)
    assert.equal(sendErrBanner(), null, '送成功後說明收掉')
  } finally {
    globalThis.fetch = fetchBefore
    restore()
  }
})
