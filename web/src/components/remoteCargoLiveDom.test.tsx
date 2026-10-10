/**
 * 外部 Cargo 面板的結果區塊是 live region（#1221）：容器要一開始就在 DOM 裡，儲存／測試／安裝的結果填進去時螢幕閱讀器才唸得到；
 * 失敗用 role=alert，成功只是 status。真的掛進 happy-dom，後端是 mock；PUT 失敗用 globalThis.fetch 包一層模擬。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest } from '../store/store'
import { HostsPanel } from './HostsPanel'

const mock = sharedMock

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const panel = () => document.querySelector('.remote-cargo-settings')!
const btn = (text: string) => [...panel().querySelectorAll('button')].find((b) => b.textContent?.includes(text))
const live = () => panel().querySelector('[role="status"][aria-live="polite"]')!

async function open(saved: Record<string, unknown> = {}) {
  await mock.request('PUT', '/build/remote', { enabled: true, host: 'build.example', user: 'builder', ssh_port: 22, remote_root: '', cargo_jobs: 4, password: '', ...saved })
  mockApi(mock)
  await mount(<HostsPanel />)
  await until(() => document.querySelector('.remote-cargo-settings') !== null, '面板讀完設定')
}

/** 只讓 PUT /build/remote 回 status，其餘照常走 mock。 */
function failPutWith(status: number): () => void {
  const realFetch = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: RequestInit) => {
    if (String(input) === '/api/build/remote' && init?.method === 'PUT') {
      return new Response(JSON.stringify({ message: 'mock: 寫入失敗' }), { status })
    }
    return realFetch(input, init)
  }) as unknown as typeof fetch
  return () => {
    globalThis.fetch = realFetch
  }
}

it('結果區塊一開始就在 DOM 裡（空的），還沒按任何按鈕時不唸、不佔版面（#1221）', async () => {
  await open()
  assert.ok(live(), 'live region 常駐')
  assert.equal(live().textContent?.trim(), '', '還沒有結果')
  assert.equal(panel().querySelector('pre.host-result'), null)
})

it('儲存失敗：訊息是 alert，以「儲存失敗：」開頭（#1221）', async () => {
  await open()
  const restore = failPutWith(500)
  try {
    await click(btn('儲存')!)
    await until(() => panel().querySelector('[role="alert"]') !== null, '失敗訊息是 alert')
    const alert = panel().querySelector('[role="alert"]')!
    assert.match(alert.textContent ?? '', /^儲存失敗：/)
    assert.ok(live().contains(alert), '失敗訊息也在 live region 裡')
  } finally {
    restore()
  }
})

it('儲存成功：訊息在 status 裡、不是 alert（欄位問題清單的 ul[role=alert] 不算）（#1221）', async () => {
  await open()
  await click(btn('儲存')!)
  await until(() => live().textContent?.includes('已儲存') === true, '已儲存訊息進 status')
  assert.equal(panel().querySelector('pre.host-result[role="alert"]'), null, '成功不是 alert')
  assert.equal(panel().querySelector('[role="alert"]'), null)
})
