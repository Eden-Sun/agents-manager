/** agy 額度（SPEC §12a.7）：只顯示 Gemini key 的 5h／7d，完整、手機與收合規則沿用 claude 格。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { weeklyOnlyKind } from '../store/quotaLookup'
import { QuotaStrip } from './QuotaStrip'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const hoursAhead = (h: number) => new Date(Date.now() + h * 3_600_000).toISOString()
const quotaWindow = (remaining: number, h: number) => ({
  used_pct: 100 - remaining,
  resets_at: hoursAhead(h),
  low: remaining < 30,
  critical: remaining < 5,
  observed_at: null,
})
const agyQuota = (five: number, week: number, fiveReset = 4, weekReset = 100, limit_hit: { message: string; until: string; at: string; bucket: string | null } | null = null) => ({
  five_hour: quotaWindow(five, fiveReset),
  seven_day: quotaWindow(week, weekReset),
  fable: null,
  reset_credits: null,
  limit_hit,
  plan: null,
  updated_at: new Date().toISOString(),
  source: 'agy-usage',
  account: null,
  host: 'local',
  stale: false,
})

test('weeklyOnlyKind：只有 grok 是週窗專用，agy 與 claude 一樣有 5h／7d', () => {
  assert.equal(weeklyOnlyKind('agy'), false)
  assert.equal(weeklyOnlyKind('grok'), true)
  assert.equal(weeklyOnlyKind('claude'), false)
  assert.equal(weeklyOnlyKind('codex'), false)
})

test('agy 顯示 Gemini 的 5h／7d，忽略舊 Claude/GPT key，popover 保留登出與撞限明細', { timeout: 30_000 }, async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await act(async () => {
    useStore.setState((s) => ({ localTools: { ...s.localTools, agy: { ...s.localTools.agy, logged_in: true } } }))
    useStore.setState((s) => ({
      quota: {
        ...s.quota,
        agy: agyQuota(12, 98, 4, 100, {
          message: 'Individual quota reached for Gemini Pro. Resets in 1h 30m',
          until: hoursAhead(1.5),
          at: new Date().toISOString(),
          bucket: 'five_hour',
        }),
        'agy:claude-gpt': agyQuota(70, 30),
      },
    }) as never)
  })
  await mount(<QuotaStrip />)
  await until(() => document.querySelectorAll('.quota-hp.agy').length === 1, '一格 agy')
  await act(async () => {})
  assert.equal(document.querySelectorAll('.quota-hp.agy').length, 1, '舊子 key 不另開一格')
  const cell = document.querySelector<HTMLElement>('.quota-hp.agy')!
  const title = cell.getAttribute('title') ?? ''
  assert.match(title, /5 小時剩餘 12%/)
  assert.match(title, /7 天剩餘 98%/)
  assert.doesNotMatch(title, /Claude\+GPT|C\+G/)
  assert.match(title, /Individual quota reached for Gemini Pro/)
  assert.ok(cell.classList.contains('quota-blocked'), 'agy 撞限需要可見的被擋提示')
  const collapsed = document.querySelector('.quota-open')?.classList.contains('collapsed')
  const names = [...cell.querySelectorAll('.quota-window-name')].map((n) => n.textContent)
  assert.deepEqual(names, collapsed ? ['5h'] : ['5h', '7d'], cell.innerHTML)
  assert.ok(cell.querySelector('.quota-cell-toggle'), '整格照 kind 可以停用')

  await click(cell.querySelector('.quota-bars-open')!)
  await until(() => document.querySelector('.quota-pop') !== null, 'popover 開啟')
  const detail = [...document.querySelectorAll('.quota-pop-row')].find((r) => r.querySelector('.quota-kind.agy'))!
  const text = detail.textContent ?? ''
  assert.match(text, /Gemini · 5h/)
  assert.match(text, /Gemini · 7d/)
  assert.doesNotMatch(text, /Claude\+GPT|C\+G/)
  assert.match(text, /CLI 回報額度上限/)
  assert.ok([...detail.querySelectorAll('button')].some((b) => b.textContent?.includes('登出 agy')), '登出按鈕保留')
})

test('agy 手機視圖沿用 claude 的緊縮規則並保留 5h／7d 邊框提示', { timeout: 30_000 }, async () => {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  try {
    mockApi(sharedMock)
    await useStore.getState().refreshState()
    await useStore.getState().loadQuota()
    await act(async () => {
      useStore.setState((s) => ({ localTools: { ...s.localTools, agy: { ...s.localTools.agy, logged_in: true } } }))
      useStore.setState((s) => ({ quota: { ...s.quota, agy: agyQuota(12, 98) } }) as never)
    })
    await mount(<QuotaStrip />)
    await until(() => document.querySelector('.quota-hp.agy') !== null, '一格 agy')
    const cell = document.querySelector<HTMLElement>('.quota-hp.agy')!
    assert.deepEqual([...cell.querySelectorAll('.quota-compact-win .quota-window-name')].map((n) => n.textContent), ['5h'])
    assert.equal(cell.querySelectorAll('.quota-border-meter').length, 2, '兩個窗口都保留邊框提示')
    assert.match(cell.querySelector('.quota-border-meter.top')?.getAttribute('aria-label') ?? '', /5H/)
    assert.match(cell.querySelector('.quota-border-meter.bottom')?.getAttribute('aria-label') ?? '', /7D/)
  } finally {
    window.matchMedia = original
  }
})
