import { test } from 'node:test'
import assert from 'node:assert/strict'
import { resetBadge } from './quotaReset.ts'

const at = (h: number, m: number) => new Date(2026, 8, 14, h, m, 0).toISOString()
const now = new Date(2026, 8, 14, 0, 44, 0).getTime()

test('歸零且三小時內重置：只寫倒數（使用者 2026-09-14：不要重置時刻）', () => {
  assert.equal(resetBadge(0, at(3, 25), now), '2h41m')
  assert.equal(resetBadge(0, at(0, 56), now), '12m')
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
