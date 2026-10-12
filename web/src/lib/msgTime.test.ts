import test from 'node:test'
import assert from 'node:assert/strict'
import { dateTimeLabel, msgTimeLabel, msgTimeTitle, nextLocalMidnight, startOfLocalDay } from './msgTime.ts'

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

test('任意時刻的日期時間（任務完成時間等）：同年只寫月日，跨年才帶年', () => {
  const now = new Date(2027, 0, 1, 0, 30).getTime()
  assert.equal(dateTimeLabel(new Date(2026, 11, 31, 23, 50).toISOString(), now), '2026/12/31 23:50')
  assert.equal(dateTimeLabel(new Date(2027, 0, 1, 0, 10).toISOString(), now), '1/1 00:10')
  assert.equal(dateTimeLabel('x', now), '')
})

test('本地「今天的開始」與下一個午夜：跨日後頁面要換成新的一天', () => {
  const now = new Date(2026, 11, 31, 22, 0).getTime()
  assert.equal(startOfLocalDay(now), new Date(2026, 11, 31).getTime())
  assert.equal(nextLocalMidnight(now), new Date(2027, 0, 1).getTime())
  // 正好在午夜：下一個午夜是明天的明天。
  assert.equal(nextLocalMidnight(new Date(2027, 0, 1, 0, 0).getTime()), new Date(2027, 0, 2).getTime())
})
