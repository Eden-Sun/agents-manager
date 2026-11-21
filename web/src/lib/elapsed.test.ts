import test from 'node:test'
import assert from 'node:assert/strict'
import { activityStartedAt, fmtElapsed } from './elapsed.ts'

test('跑了多久：秒、分秒、時分秒', () => {
  assert.equal(fmtElapsed(14_000), '14s')
  assert.equal(fmtElapsed(194_000), '3m14')
  assert.equal(fmtElapsed(185_000), '3m05')
  assert.equal(fmtElapsed(4_805_000), '1h20')
  assert.equal(fmtElapsed(3_600_000), '1h00')
  assert.equal(fmtElapsed(8_455_000), '2h20')
})

test('「開始跑」的起點：daemon 的 agent_status_since 優先，turn 時間墊後，兩個都沒有才交回呼叫端', () => {
  const turns = [
    { status: 'completed', created_at: '2026-09-18T00:00:01.000Z' },
    { status: 'in_flight', created_at: '2026-09-18T00:00:05.000Z' },
    { status: 'in_flight', created_at: '2026-09-18T00:00:03.000Z' },
  ]
  assert.equal(activityStartedAt('2026-09-18T00:00:00.000Z', turns), '2026-09-18T00:00:00.000Z')
  assert.equal(activityStartedAt(null, turns), '2026-09-18T00:00:03.000Z')
  assert.equal(activityStartedAt(undefined, turns), '2026-09-18T00:00:03.000Z')
  assert.equal(activityStartedAt(null, []), null)
  assert.equal(
    activityStartedAt(
      null,
      turns.filter((t) => t.status !== 'in_flight'),
    ),
    null,
  )
})
