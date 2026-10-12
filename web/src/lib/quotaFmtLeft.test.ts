import test from 'node:test'
import assert from 'node:assert/strict'
import { fmtLeft, fmtLeftLong } from './quotaReset.ts'

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
