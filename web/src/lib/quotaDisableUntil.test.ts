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

test('resets_at 已經過去或壞掉的不算', () => {
  assert.equal(disableUntil(quota(win(-1), win(72)), now), at(72), '過去的 5h 不拿來當最早')
  assert.equal(disableUntil(quota(win(-1, { critical: true }), win(72)), now), at(72), 'critical 但 reset 已過：退回其他窗口')
  assert.equal(disableUntil(quota(win(0), win(-5)), now), null, '剛好等於現在也算過去')
  const bad = win(2)
  bad.resets_at = 'not a date'
  assert.equal(disableUntil(quota(bad, win(72, { critical: true })), now), at(72))
})
