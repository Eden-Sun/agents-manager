import test from 'node:test'
import assert from 'node:assert/strict'
import { KICK_REQUESTER, oldestWaitMinutes, pendingRebuilds, type RebuildRequest } from './rebuildCount.ts'

const row = (o: Partial<RebuildRequest>): RebuildRequest => ({
  id: 'x',
  requester: 'bot-1',
  target_commit: 'c1',
  scope: '',
  status: 'pending',
  created_at: '2026-09-14T00:00:00.000Z',
  ...o,
})

test('只算還在等的：已經被決定掉的不是請求', () => {
  const rows = [
    row({ id: 'a' }),
    row({ id: 'b', requester: 'bot-2', status: 'approved' }),
    row({ id: 'c', requester: 'bot-3', status: 'denied' }),
    row({ id: 'd', requester: 'bot-4', status: 'consumed' }),
  ]
  assert.deepEqual(pendingRebuilds(rows).map((r) => r.id), ['a', 'b'])
})

test('同一個 requester 對同一個 commit 只算一筆', () => {
  const rows = [row({ id: 'a' }), row({ id: 'a2' }), row({ id: 'b', target_commit: 'c2' })]
  assert.equal(pendingRebuilds(rows).length, 2)
})

test('新的排前面：chip 點開先看到最新那筆', () => {
  const rows = [
    row({ id: 'old', created_at: '2026-09-14T00:00:00.000Z' }),
    row({ id: 'new', requester: 'bot-9', created_at: '2026-09-14T02:00:00.000Z' }),
  ]
  assert.deepEqual(pendingRebuilds(rows).map((r) => r.id), ['new', 'old'])
})

test('上次上線之前建立的申請不算（daemon 有 last_deploy 時）', () => {
  const rows = [
    row({ id: 'old', created_at: '2026-09-14T00:00:00.000Z' }),
    row({ id: 'new', requester: 'bot-2', created_at: '2026-09-14T03:00:00.000Z' }),
  ]
  assert.deepEqual(pendingRebuilds(rows, '2026-09-14T01:00:00.000Z').map((r) => r.id), ['new'])
  // 舊 daemon 沒有那個欄位：不濾時間，兩筆都算。
  assert.equal(pendingRebuilds(rows, null).length, 2)
})

test('時間壞掉就留著：少算一筆比多算一筆難查', () => {
  const rows = [row({ id: 'bad', created_at: 'not-a-date' })]
  assert.equal(pendingRebuilds(rows, '2026-09-14T01:00:00.000Z').length, 1)
  assert.equal(pendingRebuilds(rows, 'also-bad').length, 1)
})

test('最早那筆等了幾分鐘：看最舊的，壞時間不算，未來時間當 0', () => {
  const now = Date.parse('2026-09-15T01:00:00.000Z')
  const rows = [
    row({ id: 'new', created_at: '2026-09-15T00:50:00.000Z' }),
    row({ id: 'old', created_at: '2026-09-15T00:15:00.000Z' }),
    row({ id: 'bad', created_at: 'not-a-date' }),
  ]
  assert.equal(oldestWaitMinutes(rows, now), 45)
  assert.equal(oldestWaitMinutes([], now), 0)
  assert.equal(oldestWaitMinutes([row({ created_at: '2026-09-15T02:00:00.000Z' })], now), 0)
})

test('過期的申請不算：daemon 不會把過期的核准改狀態，chip 不能拿它說「馬上要重建」', () => {
  const now = Date.parse('2026-09-16T12:00:00.000Z')
  const rows = [
    row({ id: 'expired', expires_at: '2026-09-16T11:00:00.000Z' }),
    row({ id: 'live', requester: 'bot-2', expires_at: '2026-09-16T13:00:00.000Z' }),
    row({ id: 'forever', requester: 'bot-3' }),
    row({ id: 'bad-expiry', requester: 'bot-4', expires_at: 'not-a-date' }),
  ]
  assert.deepEqual(pendingRebuilds(rows, null, now).map((r) => r.id), ['expired', 'live', 'forever', 'bad-expiry'].filter((id) => id !== 'expired'))
})

test('腳本自己提的那筆不算（跟 kick 腳本同一條規則）', () => {
  const rows = [row({ id: 'mine', requester: KICK_REQUESTER }), row({ id: 'bot', requester: 'bot-9' })]
  assert.deepEqual(pendingRebuilds(rows).map((r) => r.id), ['bot'])
})
