import test from 'node:test'
import assert from 'node:assert/strict'
import { useStore } from '../store/store'
import { fmtDateTime, fmtTime, userTimeZone } from './userTime'
import { isValidTimeZone, utcOffsetLabel } from './timeZone'

const OPTS: Intl.DateTimeFormatOptions = { hour12: false, hour: '2-digit', minute: '2-digit' }

test('userTime：同一個 Date 在 Asia/Taipei 與 America/New_York 顯示不同', () => {
  const d = new Date('2026-11-17T04:00:00Z')
  useStore.setState({ userTimeZone: 'Asia/Taipei' })
  assert.equal(userTimeZone(), 'Asia/Taipei')
  const taipei = fmtDateTime(d, OPTS)
  const taipeiTime = fmtTime(d, OPTS)
  useStore.setState({ userTimeZone: 'America/New_York' })
  const ny = fmtDateTime(d, OPTS)
  assert.notEqual(taipei, ny)
  assert.match(taipei, /12:00/)
  assert.match(taipeiTime, /12:00/)
  assert.match(ny, /23:00/)
  useStore.setState({ userTimeZone: 'Asia/Taipei' })
})

test('utcOffsetLabel：daemon 同款寫法（+8、-5、+5:30、+0）', () => {
  const at = Date.parse('2026-11-17T04:00:00Z')
  assert.equal(utcOffsetLabel('Asia/Taipei', at), '+8')
  assert.equal(utcOffsetLabel('America/New_York', at), '-5')
  assert.equal(utcOffsetLabel('Asia/Kolkata', at), '+5:30')
  assert.equal(utcOffsetLabel('UTC', at), '+0')
  assert.equal(utcOffsetLabel('Not/AZone', at), '')
})

test('isValidTimeZone：認得 IANA 名稱，不認得的退回 false', () => {
  assert.equal(isValidTimeZone('Asia/Taipei'), true)
  assert.equal(isValidTimeZone('Mars/Olympus'), false)
  assert.equal(isValidTimeZone(''), false)
})
