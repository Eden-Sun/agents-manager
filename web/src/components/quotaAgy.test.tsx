/** agy 額度（SPEC §12a.7）：Gemini 與 Claude+GPT 各有 5h／週兩窗；主格各顯示較緊的一窗，popover 列完整明細。 */
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

test('weeklyOnlyKind：agy weekly 窗標「週」，grok 仍然只有週窗', () => {
  assert.equal(weeklyOnlyKind('agy'), true)
  assert.equal(weeklyOnlyKind('grok'), true)
  assert.equal(weeklyOnlyKind('claude'), false)
  assert.equal(weeklyOnlyKind('codex'), false)
})

test('agy 各模型組顯示較緊的 5h／週窗口，popover 顯示四窗', { timeout: 30_000 }, async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await act(async () => {
    useStore.setState((s) => ({ quota: { ...s.quota, agy: agyQuota(12, 98, 4, 100, {
      message: 'Individual quota reached for Gemini Pro. Resets in 1h 30m',
      until: hoursAhead(1.5),
      at: new Date().toISOString(),
      bucket: 'five_hour',
    }), 'agy:claude-gpt': agyQuota(70, 30) } }) as never)
  })
  await mount(<QuotaStrip />)
  await until(() => document.querySelectorAll('.quota-hp.agy').length === 1, '一格 agy')
  await act(async () => {})
  assert.equal(document.querySelectorAll('.quota-hp.agy').length, 1, '不再拆成兩格')
  const cell = document.querySelector<HTMLElement>('.quota-hp.agy')!
  const title = cell.getAttribute('title') ?? ''
  assert.match(title, /Gemini 5h剩餘 12%/)
  assert.match(title, /Claude\+GPT 每週剩餘 30%/)
  assert.match(title, /Individual quota reached for Gemini Pro/)
  assert.ok(cell.classList.contains('quota-blocked'), 'agy 撞限需要可見的被擋提示')
  const names = [...cell.querySelectorAll('.quota-window-name')].map((n) => n.textContent)
  assert.deepEqual(names, ['G 5h', 'C+G 週'], cell.innerHTML)
  assert.ok(cell.querySelector('.quota-cell-toggle'), '整格照 kind 可以停用')

  await click(cell.querySelector('.quota-bars-open')!)
  await until(() => document.querySelector('.quota-pop') !== null, 'popover 開啟')
  const detail = [...document.querySelectorAll('.quota-pop-row')].find((r) => r.querySelector('.quota-kind.agy'))!
  const text = detail.textContent ?? ''
  assert.match(text, /Gemini · 5h/)
  assert.match(text, /Gemini · 每週/)
  assert.match(text, /Claude\+GPT · 5h/)
  assert.match(text, /Claude\+GPT · 每週/)
  assert.match(text, /CLI 回報額度上限/)
})
