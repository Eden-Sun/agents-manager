/**
 * 額度欄 agy 那格的登出（使用者 2026-10-05）：popover 的「登出 agy」→ 確認框 → `POST /api/hosts/{name}/agy/logout`，
 * 成功後那格顯示未登入。真的掛 `QuotaStrip` 進 happy-dom，後端是 mock；手機寬度（點一格只看那一格）也要點得到。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)
const btn = (root: ParentNode, text: string) => [...root.querySelectorAll('button')].find((b) => b.textContent?.includes(text))
const sent = (requests: { method: string; path: string }[]) => requests.filter((r) => r.method === 'POST' && /\/hosts\/local\/agy\/logout$/.test(r.path))

/** 共用的 mock 是行程單例，上一個測試登出過就留著；每次開頭放回「已登入、5h／7d 都有讀數」。 */
function resetMockAgy() {
  const m = sharedMock as unknown as {
    localTools: { agy: { logged_in: boolean | null } }
    quota: Record<string, unknown>
  }
  m.localTools.agy.logged_in = true
  m.quota.agy = {
    five_hour: { used_pct: 18, resets_at: new Date(Date.now() + 4 * 3_600_000).toISOString() },
    seven_day: { used_pct: 22, resets_at: new Date(Date.now() + 120 * 3_600_000).toISOString() },
    plan: 'Pro',
    updated_at: new Date().toISOString(),
    host: 'local',
  }
}

async function openAgyPopover(phone: boolean) {
  resetMockAgy()
  const original = window.matchMedia
  if (phone) {
    window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  }
  const requests = mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await act(async () => {
    useStore.setState((st) => ({ localTools: { ...st.localTools, agy: { ...st.localTools.agy, logged_in: true } } }))
  })
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-hp.agy') !== null, 'agy 那格畫出來')
  await click(phone ? document.querySelector('.quota-hp.agy')! : document.querySelector('.quota-bars-open')!)
  await until(() => document.querySelector('.quota-pop') !== null, 'popover 開了')
  return { requests, restore: () => (window.matchMedia = original) }
}

const agyRow = () => [...document.querySelectorAll('.quota-pop-row')].find((r) => r.querySelector('.quota-kind.agy'))!

for (const phone of [false, true]) {
  it(`${phone ? '手機' : '桌機'}：點 agy 那格 → 登出 agy → 先跳確認（說明會清憑證）→ 確認後打 POST、那格變未登入`, async () => {
    const { requests, restore } = await openAgyPopover(phone)
    try {
      assert.equal(useStore.getState().localTools.agy.logged_in, true, '起點：已登入')
      const logout = btn(agyRow(), '登出 agy')!
      assert.ok(logout, 'agy 那格有登出鈕')
      assert.equal(logout.disabled, false)
      await click(logout)
      const dialog = await until(() => document.querySelector('[role=dialog][aria-labelledby], .confirm-dialog') !== null, '確認框出現').then(() => document.body.textContent ?? '')
      assert.match(dialog, /會刪掉本機上 agy 的登入憑證/)
      assert.equal(sent(requests).length, 0, '還沒確認不能送')
      const confirm = [...document.querySelectorAll('button')].filter((b) => b.textContent === '登出').pop()!
      await click(confirm)
      await until(() => sent(requests).length === 1, '送出 POST /hosts/local/agy/logout')
      await until(() => useStore.getState().localTools.agy.logged_in === false, '工具狀態變未登入')
      assert.equal(useStore.getState().quota.agy, null)
      await act(async () => {})
      assert.match(agyRow().textContent ?? '', /未登入/, '那格顯示未登入')
      assert.equal(btn(agyRow(), '登出 agy'), undefined, '已未登入就沒有登出鈕')
    } finally {
      restore()
    }
  })
}

it('確認框按取消：什麼都不送、狀態不變', async () => {
  const { requests, restore } = await openAgyPopover(false)
  try {
    await click(btn(agyRow(), '登出 agy')!)
    await until(() => btn(document.body, '取消') !== undefined, '確認框出現')
    await click(btn(document.body, '取消')!)
    assert.equal(sent(requests).length, 0)
    assert.equal(useStore.getState().localTools.agy.logged_in, true)
  } finally {
    restore()
  }
})

it('popover 在確認框出現時被「點外面」關掉，確認框仍在、確認仍會送出（框的開關放在 QuotaStrip 外層）', async () => {
  const { requests, restore } = await openAgyPopover(false)
  try {
    await click(btn(agyRow(), '登出 agy')!)
    await until(() => btn(document.body, '取消') !== undefined, '確認框出現')
    await act(async () => {
      document.body.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
    })
    await until(() => document.querySelector('.quota-pop') === null, 'popover 被關掉')
    await click([...document.querySelectorAll('button')].filter((b) => b.textContent === '登出').pop()!)
    await until(() => sent(requests).length === 1, '確認框還在，照樣送出')
  } finally {
    restore()
  }
})
