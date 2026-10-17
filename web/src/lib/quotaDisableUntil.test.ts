import { test } from 'node:test'
import assert from 'node:assert/strict'
import type { KindQuota, QuotaWindow } from '../api/types'
import { disableUntil } from './quotaDisableUntil.ts'

const now = Date.UTC(2026, 9, 8, 0, 0, 0)
const inHours = (h: number) => new Date(now + h * 3_600_000).toISOString()
const win = (resetsInHours: number | null, flags: Partial<Pick<QuotaWindow, 'low' | 'critical'>> = {}): QuotaWindow => ({
  used_pct: flags.critical ? 100 : flags.low ? 80 : 10,
  resets_at: resetsInHours === null ? null : inHours(resetsInHours),
  observed_at: null,
  low: false,
  critical: false,
  ...flags,
})
const quota = (five: QuotaWindow | null, seven: QuotaWindow | null, fable: QuotaWindow | null = null): KindQuota => ({
  five_hour: five,
  seven_day: seven,
  fable,
  reset_credits: null,
  limit_hit: null,
  plan: null,
  updated_at: new Date(now).toISOString(),
  stale: false,
  host: 'local',
})
const at = (h: number) => now + h * 3_600_000

test('7d 用完、5h 正常：取 7d 的 reset，不是比較早的 5h', () => {
  assert.equal(disableUntil(quota(win(2), win(72, { critical: true, low: true })), now), at(72))
})

test('5h 用完、7d 正常：取 5h 的 reset', () => {
  assert.equal(disableUntil(quota(win(2, { critical: true, low: true }), win(72)), now), at(2))
})

test('兩個都 critical：取較晚的', () => {
  assert.equal(disableUntil(quota(win(2, { critical: true }), win(72, { critical: true })), now), at(72))
  assert.equal(disableUntil(quota(win(5, { critical: true }), win(1, { critical: true })), now), at(5))
})

test('Fable 週窗用完也算', () => {
  assert.equal(disableUntil(quota(win(2), win(70), win(100, { critical: true })), now), at(100))
})

test('沒有 critical 但有 low：取 low 窗口中最晚的', () => {
  assert.equal(disableUntil(quota(win(2, { low: true }), win(72)), now), at(2))
  assert.equal(disableUntil(quota(win(2, { low: true }), win(72, { low: true })), now), at(72))
})

test('critical 優先於 low：低的窗口不拖長也不縮短', () => {
  assert.equal(disableUntil(quota(win(2, { critical: true }), win(72, { low: true })), now), at(2))
})

test('都正常（只是想先收起來）：最早的 reset', () => {
  assert.equal(disableUntil(quota(win(2), win(72), win(100)), now), at(2))
  assert.equal(disableUntil(quota(null, win(72)), now), at(72))
})

test('沒有 resets_at、沒有額度資料：null（只能手動解除）', () => {
  assert.equal(disableUntil(quota(win(null, { critical: true }), win(null)), now), null)
  assert.equal(disableUntil(quota(null, null), now), null)
  assert.equal(disableUntil(null, now), null)
  assert.equal(disableUntil(undefined, now), null)
})

test('被選為阻擋的窗口 reset 已過或壞掉：回 null，不借別的窗口', () => {
  assert.equal(disableUntil(quota(win(-1), win(72)), now), at(72), '都正常時，過去的 5h 忽略，取有效的 7d')
  assert.equal(disableUntil(quota(win(-1, { critical: true }), win(72)), now), null, 'critical 但 reset 已過：不退回健康的 7d')
  assert.equal(disableUntil(quota(win(0), win(-5)), now), null, '剛好等於現在也算過去')
  const bad = win(2)
  bad.resets_at = 'not a date'
  assert.equal(disableUntil(quota(bad, win(72, { critical: true })), now), at(72), '阻擋的是 7d，5h 壞掉不影響')
  assert.equal(disableUntil(quota(win(2), bad, win(100, { critical: true })), now), at(100), '阻擋的是 fable，5h 壞掉不影響')
})

test('7d 用完但 reset 未知、5h 正常有 reset：null，不准用 5h 的 reset 提前解除', () => {
  assert.equal(disableUntil(quota(win(2), win(null, { critical: true, low: true })), now), null)
  assert.equal(disableUntil(quota(win(2), win(-1, { critical: true, low: true })), now), null)
})

test('兩個 critical 其中一個 reset 未知：null', () => {
  assert.equal(disableUntil(quota(win(2, { critical: true }), win(null, { critical: true })), now), null)
  assert.equal(disableUntil(quota(win(null, { critical: true }), win(72, { critical: true })), now), null)
})

test('critical reset 已知、low 的 reset 未知：只看 critical，取它的 reset', () => {
  assert.equal(disableUntil(quota(win(2, { critical: true }), win(null, { low: true })), now), at(2))
})

test('沒有 critical，low 的 reset 未知：null（不借健康窗口）', () => {
  assert.equal(disableUntil(quota(win(2), win(null, { low: true })), now), null)
  assert.equal(disableUntil(quota(win(2, { low: true }), win(null, { low: true }), null), now), null)
})

test('都正常、全部 reset 已知：取最早', () => {
  assert.equal(disableUntil(quota(win(72), win(2), win(24)), now), at(2))
})

test('都正常、其中一個 reset 缺失：忽略它，取其他有效的最早', () => {
  assert.equal(disableUntil(quota(win(2), win(null)), now), at(2))
  assert.equal(disableUntil(quota(win(null), win(72), win(24)), now), at(24))
})

test('都正常、所有 reset 都無效：null', () => {
  assert.equal(disableUntil(quota(win(null), win(-1)), now), null)
})
