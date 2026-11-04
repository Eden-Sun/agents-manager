import test from 'node:test'
import assert from 'node:assert/strict'
import { leftPct } from './quotaLeft.ts'

test('剩餘 % 不到 10 留一位小數，10 以上取整數', () => {
  assert.equal(leftPct(98.4), 1.6)
  assert.equal(leftPct(90.2), 9.8)
  assert.equal(leftPct(90), 10)
  assert.equal(leftPct(85.4), 15)
})

test('剩餘 % 不會是負的', () => {
  assert.equal(leftPct(100), 0)
  assert.equal(leftPct(103), 0)
})
