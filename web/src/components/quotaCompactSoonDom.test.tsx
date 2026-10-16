/**
 * 手機（compact）：5h 用完、40 分鐘後重置時，已經寫倒數（0h40）就不再把窗口名換成「40m」——
 * 同一個時間寫兩次（2026-10-16 使用者：「手機版倒數 分鐘顯示就重複了」）。真的掛 `QuotaStrip` 進 happy-dom。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

type MockQuota = { quota: Record<string, Record<string, unknown> | undefined> }
const mock = sharedMock as unknown as MockQuota
const originalClaude = mock.quota.claude

const cellOf = (identity: string) =>
  [...document.querySelectorAll<HTMLElement>('.quota-hp.claude')].find((el) => el.querySelector('.quota-identity')?.textContent === identity) ?? null

function phoneWidth(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

virtualMockTime()
afterEach(async () => {
  mock.quota.claude = originalClaude
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

test('手機：5h 用完 40 分鐘後重置，只寫一次時間（窗口名維持 5h）', { timeout: 30_000 }, async () => {
  resetStoreForTest()
  const restore = phoneWidth()
  try {
    const base = originalClaude ?? {}
    const at = (ms: number) => new Date(Date.now() + ms).toISOString()
    mock.quota.claude = {
      ...base,
      five_hour: { used_pct: 100, low: true, critical: true, resets_at: at(40 * 60_000), observed_at: null },
      seven_day: { used_pct: 10, low: false, critical: false, resets_at: at(3 * 86_400_000), observed_at: null },
      fable: null,
    }
    mockApi(sharedMock)
    await useStore.getState().refreshState()
    await useStore.getState().loadQuota()
    await mount(<QuotaStrip />)
    await until(() => cellOf('cc0') !== null, 'cc0 格畫出來')
    const win = cellOf('cc0')!.querySelector<HTMLElement>('.quota-compact-win')!
    assert.equal(win.querySelector('.quota-window-name')?.textContent, '5h', '窗口名不換成剩幾分')
    assert.match(win.querySelector('.quota-reset-at')?.textContent ?? '', /^0h(39|40)$/, '倒數寫一次')
    assert.doesNotMatch(win.textContent ?? '', /\d+m/, '不再出現「40m」')
  } finally {
    restore()
  }
})
