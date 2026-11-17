import test from 'node:test'
import assert from 'node:assert/strict'
import { useStore } from '../store/store'
import { msgTimeLabel, msgTimeTitle, nextLocalMidnight, startOfLocalDay } from './msgTime'
import { sameZonedDay, zonedDayStart, zonedParts } from './userTime'

// 同一個 UTC 時刻：台北是 12:00（同一天），紐約是 23:00（前一天）。
const UTC = '2026-11-17T04:00:00Z'
const NOW = Date.parse('2026-11-17T06:00:00Z')

test('msgTimeLabel：同一個 UTC 時刻在 Asia/Taipei 與 America/New_York 給不同 HH:mm 與不同「同日」判斷', () => {
  useStore.setState({ userTimeZone: 'Asia/Taipei' })
  assert.equal(msgTimeLabel(UTC, NOW), '12:00')
  assert.equal(msgTitleOf('Asia/Taipei'), '2026-11-17 12:00:00')
  useStore.setState({ userTimeZone: 'America/New_York' })
  // 紐約現在是 2026-11-17 01:00，訊息是 11/16 23:00：不同日 → 帶月日。
  assert.equal(msgTimeLabel(UTC, NOW), '11/16 23:00')
  assert.equal(msgTitleOf('America/New_York'), '2026-11-16 23:00:00')
  assert.equal(sameZonedDay(Date.parse(UTC), NOW, 'America/New_York'), false)
  assert.equal(sameZonedDay(Date.parse(UTC), NOW, 'Asia/Taipei'), true)
  useStore.setState({ userTimeZone: 'Asia/Taipei' })
})

function msgTitleOf(zone: string): string {
  return msgTimeTitle(UTC, zone)
}

test('zonedDayStart：跨日界正確（台北 00:00 ＝ 前一天 UTC 16:00）', () => {
  const taipei = zonedDayStart(NOW, 'Asia/Taipei')
  assert.equal(new Date(taipei).toISOString(), '2026-11-16T16:00:00.000Z')
  assert.equal(zonedParts(taipei, 'Asia/Taipei').hour, 0)
  // 午夜前一秒仍是當天，午夜那一刻換天。
  const beforeMidnight = Date.parse('2026-11-16T15:59:59Z')
  assert.equal(zonedDayStart(beforeMidnight, 'Asia/Taipei'), taipei - 86_400_000)
  assert.equal(zonedDayStart(Date.parse('2026-11-16T16:00:00Z'), 'Asia/Taipei'), taipei)
  assert.equal(nextLocalMidnight(NOW, 'Asia/Taipei'), taipei + 86_400_000)
  assert.equal(startOfLocalDay(NOW, 'Asia/Taipei'), taipei)
})
