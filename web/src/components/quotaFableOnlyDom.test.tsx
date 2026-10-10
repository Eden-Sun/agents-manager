/**
 * #1179：額度只有 Fable 有值（5h、7d 都 null）時，條上要畫出 F 的 %，不能畫成「5h —」；手機（compact）也一樣。
 * 5h、7d、F 全部沒有值時，才保留「5h —」佔位。真的掛 `QuotaStrip` 進 happy-dom，額度來自 mock。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

type MockQuota = { quota: Record<string, Record<string, unknown> | undefined> }
const mock = sharedMock as unknown as MockQuota
const originalClaude = mock.quota.claude

const cellOf = (identity: string) =>
  [...document.querySelectorAll<HTMLElement>('.quota-hp.claude')].find((el) => el.querySelector('.quota-identity')?.textContent === identity) ?? null

/** 手機寬度：`matchMedia` 對 640px 斷點回 true（與 hostShellPhoneDom.test.tsx 同寫法）。 */
function phoneWidth(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

/** 以 mock 的 claude 額度為底，換掉 5h／7d／F，再載入並畫出 cc0 格。 */
async function openWith(windows: { five: boolean; seven: boolean; fable: boolean }) {
  const base = originalClaude ?? {}
  const pick = (on: boolean, w: Record<string, unknown> | null | undefined, over: Record<string, unknown>) =>
    on ? { ...(w ?? {}), used_pct: 98, low: true, critical: true, resets_at: null, ...over } : null
  mock.quota.claude = {
    ...base,
    five_hour: pick(windows.five, base.five_hour as Record<string, unknown> | undefined, {}),
    seven_day: pick(windows.seven, base.seven_day as Record<string, unknown> | undefined, {}),
    fable: pick(windows.fable, base.fable as Record<string, unknown> | undefined, {}),
  }
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await mount(<QuotaStrip />)
  await until(() => cellOf('cc0') !== null, 'cc0 格畫出來')
  return cellOf('cc0')!
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

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

async function openOnlyFable(fable: boolean) {
  mockApi(sharedMock)
  resetStoreForTest()
  const now = new Date().toISOString()
  useStore.setState({
    quota: {
      claude: {
        five_hour: null,
        seven_day: null,
        fable: fable ? { used_pct: 20, low: false, critical: false, resets_at: null, observed_at: null } : null,
        reset_credits: null,
        limit_hit: null,
        plan: null,
        updated_at: now,
        stale: false,
        host: 'local',
      },
    },
    identities: [],
  } as never)
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-hp.claude') !== null, 'Claude 額度格畫出來')
  return document.querySelector<HTMLElement>('.quota-hp.claude')!
}

it('只有 Fable：tooltip 與讀屏名稱不寫「尚未取得」', async () => {
  const cell = await openOnlyFable(true)
  assert.ok(cell.getAttribute('title')?.includes('Fable 每週剩餘 80%'))
  assert.ok(!cell.getAttribute('title')?.includes('尚未取得'))
  assert.ok(!cell.querySelector('.quota-bars-open')?.getAttribute('aria-label')?.includes('尚未取得'))
})

it('只有 Fable：popover 畫出唯一的 Fable 列；全部沒有讀數時仍顯示尚未取得', async () => {
  const cell = await openOnlyFable(true)
  await click(cell.querySelector('.quota-bars-open')!)
  const pop = document.querySelector('.quota-pop')!
  assert.ok(pop.textContent?.includes('Fable'))
  assert.ok(!pop.textContent?.includes('尚未取得'))
  assert.equal(pop.querySelectorAll('.quota-pop-line').length, 1)

  await unmountAll()
  const emptyCell = await openOnlyFable(false)
  await click(emptyCell.querySelector('.quota-bars-open')!)
  assert.ok(document.querySelector('.quota-pop')?.textContent?.includes('尚未取得'))
})

it('桌機：只有 Fable 有值時畫出 F 的那一條，不畫「5h —」', async () => {
  resetStoreForTest()
  const cell = await openWith({ five: false, seven: false, fable: true })
  const names = [...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent)
  assert.deepEqual(names, ['F'], `只畫 F，實際 ${JSON.stringify(names)}`)
  const pct = cell.querySelector('.quota-window .quota-bar-pct')?.textContent ?? ''
  assert.ok(pct.includes('2'), `F 剩 2%，數字要寫出來，實際 ${JSON.stringify(pct)}`)
  assert.ok(!cell.textContent?.includes('5h'), '不畫 5h 佔位')
})

it('手機：只有 Fable 有值時仍只寫一個窗口，寫出 F 的 %，並帶上 F 自己的危急顏色', async () => {
  resetStoreForTest()
  const restore = phoneWidth()
  try {
    const cell = await openWith({ five: false, seven: false, fable: true })
    const wins = [...cell.querySelectorAll<HTMLElement>('.quota-compact-win')]
    assert.equal(wins.length, 1, `手機一格只寫一個窗口，實際 ${wins.length} 個`)
    assert.equal(wins[0].querySelector('.quota-window-name')?.textContent, 'F', '名稱是 F，不是 5h')
    assert.ok((wins[0].querySelector('.quota-compact-pct')?.textContent ?? '').includes('2'), '數字寫出來')
    assert.ok(wins[0].classList.contains('crit'), '顏色跟著 F 自己的 critical 旗標')
  } finally {
    restore()
  }
})

it('5h、7d、F 全部沒有值時保留「5h —」佔位', async () => {
  resetStoreForTest()
  const cell = await openWith({ five: false, seven: false, fable: false })
  const names = [...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent)
  assert.deepEqual(names, ['5h'], `佔位仍是 5h，實際 ${JSON.stringify(names)}`)
})
