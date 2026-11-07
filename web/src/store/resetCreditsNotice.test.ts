import test from 'node:test'
import assert from 'node:assert/strict'
import type { KindQuota } from '../api/types'
import { creditExpiryText, creditKeyLabel, resetCreditChanges, resetCreditExpiries } from './resetCreditsNotice.ts'

const q = (available: number | null, stale = false) =>
  ({ reset_credits: available == null ? null : { available, title: null, expires_at: null, credits: [] }, stale }) as unknown as KindQuota

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

const HOUR = 3600_000
const NOW = Date.parse('2026-12-01T00:00:00Z')
const withCredits = (credits: { id: string; status?: string; hours: number | null }[], stale = false) =>
  ({
    reset_credits: {
      available: credits.length,
      title: null,
      expires_at: null,
      credits: credits.map((c) => ({
        id: c.id,
        title: 'Full reset',
        status: c.status ?? 'available',
        granted_at: null,
        expires_at: c.hours === null ? null : new Date(NOW + c.hours * HOUR).toISOString(),
      })),
    },
    stale,
  }) as unknown as KindQuota

test('到期前 24 小時內才提示，24 小時外、已過期、沒到期日、已用掉的都不提示', () => {
  const quota = { 'm4p/codex:co0': withCredits([
    { id: 'soon', hours: 23 },
    { id: 'later', hours: 25 },
    { id: 'gone', hours: -1 },
    { id: 'none', hours: null },
    { id: 'used', status: 'used', hours: 3 },
  ]) }
  const { due } = resetCreditExpiries({}, quota, NOW)
  assert.deepEqual(due.map((d) => d.id), ['soon'])
  assert.match(creditExpiryText(due[0]), /codex · co0 @m4p 的重置券『Full reset』.*到期.*Reset usage/)
})

test('同一張券（同 id）只提示一次：已記的不再提示，進入窗口才提示', () => {
  const first = resetCreditExpiries({}, { codex: withCredits([{ id: 'a', hours: 20 }, { id: 'b', hours: 30 }]) }, NOW)
  assert.deepEqual(first.due.map((d) => d.id), ['a'])
  const again = resetCreditExpiries(first.next, { codex: withCredits([{ id: 'a', hours: 19 }, { id: 'b', hours: 30 }]) }, NOW + HOUR)
  assert.deepEqual(again.due, [])
  const later = resetCreditExpiries(again.next, { codex: withCredits([{ id: 'a', hours: 5 }, { id: 'b', hours: 30 }]) }, NOW + 15 * HOUR)
  assert.deepEqual(later.due.map((d) => d.id), ['b'], 'b 現在才進 24 小時內')
  // 同一張券出現在兩個 key（預設與身份）也只提示一次。
  const twin = resetCreditExpiries({}, { codex: withCredits([{ id: 'a', hours: 5 }]), 'codex:co0': withCredits([{ id: 'a', hours: 5 }]) }, NOW)
  assert.equal(twin.due.length, 1)
})

test('stale、非 codex、舊 daemon 沒有 credits 都不提示；去重記錄超過 7 天清掉', () => {
  assert.deepEqual(resetCreditExpiries({}, { codex: withCredits([{ id: 'a', hours: 5 }], true) }, NOW).due, [])
  assert.deepEqual(resetCreditExpiries({}, { claude: withCredits([{ id: 'a', hours: 5 }]) }, NOW).due, [])
  assert.deepEqual(resetCreditExpiries({}, { codex: q(1) }, NOW).due, [])
  const { next } = resetCreditExpiries({ old: NOW - 8 * 24 * HOUR, fresh: NOW - HOUR }, {}, NOW)
  assert.deepEqual(Object.keys(next), ['fresh'])
})
