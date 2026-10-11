import test from 'node:test'
import assert from 'node:assert/strict'
import { limitHitWhen } from './limitHitWhen.ts'

test('被擋的重置時間：今天顯示時分，其他日期帶月日', () => {
  const now = new Date(2026, 9, 11, 15, 0).getTime()
  assert.equal(limitHitWhen(new Date(2026, 9, 11, 18, 30).toISOString(), now), '18:30')
  assert.equal(limitHitWhen(new Date(2026, 9, 17, 9, 0).toISOString(), now), '10/17 09:00')
  assert.equal(limitHitWhen(new Date(2026, 9, 12, 0, 5).toISOString(), now), '10/12 00:05')
})

test('被擋的重置時間沒寫或讀不懂就回 null', () => {
  assert.equal(limitHitWhen(null), null)
  assert.equal(limitHitWhen(''), null)
  assert.equal(limitHitWhen('not a date'), null)
})
