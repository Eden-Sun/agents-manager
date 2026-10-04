/**
 * agy 的額度（SPEC §12a.7）：daemon 出兩個每週桶（key `agy`＝Gemini、`agy:claude-gpt`＝Claude 與 GPT），額度條各畫一格、只有「週」窗，
 * 第二個桶不是身分所以沒有停用勾選。真的掛 `QuotaStrip` 進 happy-dom，額度直接寫進 store。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll, until, act } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { weeklyOnlyKind } from '../store/quotaLookup'
import { QuotaStrip } from './QuotaStrip'

const hoursAhead = (h: number) => new Date(Date.now() + h * 3_600_000).toISOString()
const week = (used: number, h: number) => ({ used_pct: used, resets_at: hoursAhead(h), low: false, critical: false, observed_at: null })
const agyQuota = (used: number, h: number) => ({
  five_hour: null,
  seven_day: week(used, h),
  fable: null,
  reset_credits: null,
  limit_hit: null,
  plan: null,
  updated_at: new Date().toISOString(),
  source: 'agy-usage',
  account: null,
  host: 'local',
  stale: false,
})

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

test('weeklyOnlyKind：grok 與 agy 只有週窗', () => {
  assert.equal(weeklyOnlyKind('agy'), true)
  assert.equal(weeklyOnlyKind('grok'), true)
  assert.equal(weeklyOnlyKind('claude'), false)
  assert.equal(weeklyOnlyKind('codex'), false)
})

test('agy 兩個桶合成一格：G（Gemini）剩 98%、C+G（Claude+GPT）剩 100%，都只有週窗', { timeout: 30_000 }, async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await act(async () => {
    useStore.setState((s) => ({ quota: { ...s.quota, agy: agyQuota(2, 160), 'agy:claude-gpt': agyQuota(0, 160) } }) as never)
  })
  await mount(<QuotaStrip />)
  await until(() => document.querySelectorAll('.quota-hp.agy').length === 1, '一格 agy')
  await act(async () => {})
  assert.equal(document.querySelectorAll('.quota-hp.agy').length, 1, '不再拆成兩格')
  const cell = document.querySelector<HTMLElement>('.quota-hp.agy')!
  const title = cell.getAttribute('title') ?? ''
  assert.match(title, /Gemini 每週剩餘 98%/)
  assert.match(title, /Claude\+GPT 每週剩餘 100%/)
  assert.doesNotMatch(title, /5 小時|7 天/, '只有週窗：' + title)
  const names = [...cell.querySelectorAll('.quota-window-name')].map((n) => n.textContent)
  assert.deepEqual(names, ['G', 'C+G'], cell.innerHTML)
  assert.ok(cell.querySelector('.quota-cell-toggle'), '整格照 kind 可以停用')
})
