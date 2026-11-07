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
    const win = cellOf('cc0')!.querySelector<HTMLElement>('.quota-window')!
    assert.equal(win.querySelector('.quota-window-name')?.textContent, '5h', '窗口名不換成剩幾分')
    const cell = cellOf('cc0')!
    assert.match(cell.querySelector('.quota-reset-line')?.textContent ?? '', /^└\s*0h(39|40)$/, '倒數集中寫一次')
    assert.doesNotMatch(win.textContent ?? '', /\d+m/, '不再出現「40m」')
    assert.equal(win.querySelector('.quota-bar-pct')?.textContent, '<1%', '倒數時百分比留在自己的列')
    assert.equal(cell.querySelector('.quota-head-pct'), null, '多窗口不將百分比移到 logo 列')
    assert.deepEqual([...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent), ['5h', '7d'], '較急的 5h 不遮掉週窗')
  } finally {
    restore()
  }
})

async function mountPhoneQuota(kind: string, values: Record<string, unknown>) {
  resetStoreForTest()
  mock.quota[kind] = { ...mock.quota[kind], ...values }
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await mount(<QuotaStrip />)
  await until(() => document.querySelector(`.quota-hp.${kind} .quota-mobile-bars`) !== null, '手機量表畫出來')
  return document.querySelector<HTMLElement>(`.quota-hp.${kind}`)!
}

const windowIn = (used: number, hours: number, low = false, critical = false) => ({
  used_pct: used, low, critical, resets_at: new Date(Date.now() + hours * 3_600_000).toISOString(), observed_at: null,
})

test('手機：5h、7d、F 固定順序，各自有剩量與正確重置針，週窗不被更急窗口取代', async () => {
  const restore = phoneWidth()
  try {
    const cell = await mountPhoneQuota('claude', {
      five_hour: windowIn(80, 2.5, true), seven_day: windowIn(10, 84), fable: windowIn(40, 42), limit_hit: null,
    })
    const rows = [...cell.querySelectorAll<HTMLElement>('.quota-window')]
    assert.deepEqual(rows.map((row) => row.querySelector('.quota-window-name')?.textContent), ['5h', '7d', 'F'])
    assert.deepEqual(rows.map((row) => row.querySelector('.quota-bar-pct')?.textContent), ['20%', '90%', '60%'])
    for (const [i, expected] of [50, 50, 25].entries()) {
      const mark = rows[i].querySelector<HTMLElement>('.quota-bar-mark')!
      assert.ok(Math.abs(parseFloat(mark.style.left) - expected) < 0.1)
      assert.match(mark.title, /重置/)
    }
    assert.ok(rows[0].querySelector('.quota-bar.warn'))
    assert.ok(rows[1].querySelector('.quota-bar.ok'), '週窗不被整格的警告染色')
  } finally {
    restore()
  }
})

test('手機：grok 只有週窗，仍有量表、百分比和週重置針', async () => {
  const originalGrok = mock.quota.grok
  const restore = phoneWidth()
  try {
    const cell = await mountPhoneQuota('grok', { five_hour: null, seven_day: windowIn(25, 84), fable: null, limit_hit: null })
    assert.deepEqual([...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent), ['週'])
    assert.equal(cell.querySelector('.quota-bar-pct')?.textContent, '75%')
    assert.ok(Math.abs(parseFloat(cell.querySelector<HTMLElement>('.quota-bar-mark')!.style.left) - 50) < 0.1)
  } finally {
    mock.quota.grok = originalGrok
    restore()
  }
})

test('手機：雙窗口撞限倒數仍保留各列百分比、放大條，倒數各顯示一次', async () => {
  const restore = phoneWidth()
  try {
    const cell = await mountPhoneQuota('claude', {
      five_hour: windowIn(100, 0.4, true, true), seven_day: windowIn(98, 6, true, true), fable: null,
      limit_hit: { message: 'hit', until: null, at: new Date().toISOString(), bucket: 'five_hour' },
    })
    assert.ok(cell.classList.contains('quota-blocked'))
    assert.deepEqual([...cell.querySelectorAll('.quota-bar-pct')].map((el) => el.textContent), ['0%', '2%'])
    assert.equal(cell.querySelectorAll('.quota-bar-wrap.zoomed').length, 2)
    assert.equal(cell.querySelector('.quota-bar-mark'), null, '放大刻度不混用黑針')
    assert.equal(cell.querySelectorAll('.quota-reset-line').length, 1)
    assert.match(cell.querySelector('.quota-reset-line')?.textContent ?? '', /5h.*0h(23|24).*7d.*(5h59|6h00)/)
    assert.deepEqual([...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent), ['5h', '7d'])
  } finally {
    restore()
  }
})
