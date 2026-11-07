import test from 'node:test'
import assert from 'node:assert/strict'
import type { QuotaResetCredits } from '../api/types'
import { creditRows, creditUrgent, fmtCreditIn, nearestCreditLeft } from './resetCredits.ts'

const HOUR = 3600_000
const NOW = Date.parse('2026-12-01T00:00:00Z')
const rc = (credits: { id: string; hours: number | null }[], extra: Partial<QuotaResetCredits> = {}): QuotaResetCredits => ({
  available: credits.length,
  title: null,
  expires_at: null,
  credits: credits.map((c) => ({ id: c.id, title: 'Full reset', status: 'available', granted_at: null, expires_at: c.hours === null ? null : new Date(NOW + c.hours * HOUR).toISOString() })),
  ...extra,
})

test('券依到期由早到晚排，沒到期日的排最後，已過期的不列', () => {
  const rows = creditRows(rc([{ id: 'far', hours: 700 }, { id: 'none', hours: null }, { id: 'gone', hours: -2 }, { id: 'near', hours: 5 }]), NOW)
  assert.deepEqual(rows.map((r) => r.id), ['near', 'far', 'none'])
})

test('倒數文字：天、小時、不到 1 小時；警示門檻 48 小時', () => {
  assert.equal(fmtCreditIn(24 * 24 * HOUR + 3 * HOUR), '24 天後')
  assert.equal(fmtCreditIn(47 * HOUR), '47 小時後')
  assert.equal(fmtCreditIn(20 * 60_000), '不到 1 小時')
  assert.equal(creditUrgent(47 * HOUR), true)
  assert.equal(creditUrgent(49 * HOUR), false)
  assert.equal(creditUrgent(null), false)
})

test('最近到期：有清單取最早；舊 daemon 沒清單退回 expires_at；沒券不畫', () => {
  assert.equal(nearestCreditLeft(rc([{ id: 'a', hours: 100 }, { id: 'b', hours: 30 }]), NOW), 30 * HOUR)
  const old: QuotaResetCredits = { available: 1, title: 'x', expires_at: new Date(NOW + 10 * HOUR).toISOString(), credits: [] }
  assert.equal(nearestCreditLeft(old, NOW), 10 * HOUR)
  assert.equal(nearestCreditLeft({ ...old, available: 0 }, NOW), null)
  assert.equal(nearestCreditLeft(null, NOW), null)
})
