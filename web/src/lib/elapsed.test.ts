import test from 'node:test'
import assert from 'node:assert/strict'
import { fmtElapsed } from './elapsed.ts'

test('跑了多久：秒、分秒、時分秒', () => {
  assert.equal(fmtElapsed(14_000), '14s')
  assert.equal(fmtElapsed(194_000), '3m14')
  assert.equal(fmtElapsed(185_000), '3m05')
  assert.equal(fmtElapsed(4_805_000), '1:20:05')
})
