import test from 'node:test'
import assert from 'node:assert/strict'
import { pendingRebuilds, type RebuildRequest } from './rebuildCount.ts'

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
