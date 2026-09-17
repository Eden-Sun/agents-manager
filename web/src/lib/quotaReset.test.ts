import { test } from 'node:test'
import assert from 'node:assert/strict'
import { RULE_WEEKLY, resetBadge } from './quotaReset.ts'

const at = (h: number, m: number) => new Date(2026, 8, 14, h, m, 0).toISOString()
const now = new Date(2026, 8, 14, 0, 44, 0).getTime()

test('歸零且三小時內重置：只寫倒數（使用者 2026-09-14：不要重置時刻）', () => {
  assert.equal(resetBadge(0, at(3, 25), now), '2h41')
  // 不到一小時也寫 `0h12`：旁邊全是百分比，裸的 `12` 會被讀成 12%。
  assert.equal(resetBadge(0, at(0, 56), now), '0h12')
})

test('還有額度就不寫——那時候要看的是還剩多少', () => {
  assert.equal(resetBadge(7, at(3, 25), now), null)
  assert.equal(resetBadge(100, at(3, 25), now), null)
})

test('重置在三小時之外不寫；沒有時間、時間壞掉、已經過了也不寫', () => {
  assert.equal(resetBadge(0, at(4, 0), now), null)
  assert.equal(resetBadge(0, null, now), null)
  assert.equal(resetBadge(0, 'not a date', now), null)
  assert.equal(resetBadge(0, at(0, 30), now), null)
  assert.equal(resetBadge(null, at(1, 0), now), null)
})

test('週窗口 24 小時內就寫倒數（使用者 2026-09-17）', () => {
  const inHours = (h: number) => new Date(now + h * 3_600_000).toISOString()
  assert.equal(resetBadge(0, inHours(23.5), now, RULE_WEEKLY), '23h30')
  assert.equal(resetBadge(0, inHours(4), now, RULE_WEEKLY), '4h00')
  assert.equal(resetBadge(0, inHours(25), now, RULE_WEEKLY), null)
  // 剩不到 10% 也寫（同日使用者）；10% 以上照舊寫百分比
  assert.equal(resetBadge(9, inHours(4), now, RULE_WEEKLY), '4h00')
  assert.equal(resetBadge(9.5, inHours(4), now, RULE_WEEKLY), '4h00')
  assert.equal(resetBadge(10, inHours(4), now, RULE_WEEKLY), null)
  assert.equal(resetBadge(9, inHours(25), now, RULE_WEEKLY), null)
  // 5h 門檻不變：要用完、三小時內
  assert.equal(resetBadge(0, inHours(4), now), null)
  assert.equal(resetBadge(5, inHours(1), now), null)
})
