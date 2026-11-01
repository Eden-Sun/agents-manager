import test from 'node:test'
import assert from 'node:assert/strict'
import type { KindQuota } from '../api/types'
import { creditKeyLabel, resetCreditChanges } from './resetCreditsNotice.ts'

const q = (available: number | null, stale = false) =>
  ({ reset_credits: available == null ? null : { available, title: null, expires_at: null }, stale }) as unknown as KindQuota

test('第一次看到只當基準；張數變了才回報（增減都算）', () => {
  const a = resetCreditChanges({}, { codex: q(1), 'm4p/codex:co0': q(2) })
  assert.deepEqual(a.changes, [])
  assert.deepEqual(a.next, { codex: 1, 'm4p/codex:co0': 2 })
  const b = resetCreditChanges(a.next, { codex: q(0), 'm4p/codex:co0': q(3) })
  assert.deepEqual(b.changes, [{ key: 'codex', from: 1, to: 0 }, { key: 'm4p/codex:co0', from: 2, to: 3 }])
  assert.deepEqual(resetCreditChanges(b.next, { codex: q(0) }).changes, [])
})

test('讀不到、舊讀數、非 codex 不算數，也不覆蓋基準', () => {
  const seen = { codex: 1 }
  assert.deepEqual(resetCreditChanges(seen, { codex: q(null) }), { changes: [], next: seen })
  assert.deepEqual(resetCreditChanges(seen, { codex: q(5, true) }), { changes: [], next: seen })
  assert.deepEqual(resetCreditChanges(seen, { codex: null, claude: q(9) }).changes, [])
})

test('key 轉成人看得懂的名稱', () => {
  assert.equal(creditKeyLabel('codex'), 'codex')
  assert.equal(creditKeyLabel('codex:co0'), 'codex · co0')
  assert.equal(creditKeyLabel('m4p/codex:co0'), 'codex · co0 @m4p')
})
