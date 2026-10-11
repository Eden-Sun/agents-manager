import test from 'node:test'
import assert from 'node:assert/strict'
import { msgTimeLabel, msgTimeTitle } from './msgTime.ts'

test('訊息時間標籤：今天只有時分，較早日期帶月日，其他年份帶年', () => {
  const now = new Date(2026, 9, 11, 15, 0).getTime()
  assert.equal(msgTimeLabel(new Date(2026, 9, 11, 7, 30).toISOString(), now), '07:30')
  assert.equal(msgTimeLabel(new Date(2026, 9, 10, 23, 30).toISOString(), now), '10/10 23:30')
  assert.equal(msgTimeLabel(new Date(2025, 11, 31, 9, 5).toISOString(), now), '2025/12/31 09:05')
})

test('訊息 tooltip 使用本地完整時間；讀不懂時保留原文', () => {
  assert.equal(msgTimeTitle(new Date(2026, 9, 11, 7, 30, 12).toISOString()), '2026-10-11 07:30:12')
  assert.equal(msgTimeLabel('x'), '')
  assert.equal(msgTimeTitle('x'), 'x')
})
