import test from 'node:test'
import assert from 'node:assert/strict'
import { fmtLeft, fmtLeftLong, msUntil } from './quotaReset.ts'

const MIN = 60_000
const HOUR = 60 * MIN

test('重置倒數（緊縮）：不到一分鐘也不寫 0m，跟旁邊的 resetBadge 一樣進位', () => {
  assert.equal(fmtLeft(0), '即將重置')
  assert.equal(fmtLeft(30_000), '1m')
  assert.equal(fmtLeft(12 * MIN), '12m')
  assert.equal(fmtLeft(2 * HOUR + 13 * MIN), '2h13m')
  assert.equal(fmtLeft(4 * 24 * HOUR), '4d0h')
})

test('重置倒數（長寫法）：不到一分鐘寫「1 分」，不寫「0 分」', () => {
  assert.equal(fmtLeftLong(0), '即將重置')
  assert.equal(fmtLeftLong(30_000), '1 分')
  assert.equal(fmtLeftLong(2 * HOUR + 13 * MIN), '2 小時 13 分')
  assert.equal(fmtLeftLong(2 * 24 * HOUR + 3 * HOUR), '2 天 3 小時')
})

test('距離重置還有多久：讀不出來的時間回 null（不是 NaN），空字串也一樣', () => {
  const now = Date.parse('2026-10-12T10:00:00Z')
  assert.equal(msUntil('2026-10-12T10:05:00Z', now), 5 * MIN)
  assert.equal(msUntil('not-a-date', now), null)
  assert.equal(msUntil('', now), null)
  assert.equal(msUntil(null, now), null)
  assert.equal(msUntil(undefined, now), null)
})
