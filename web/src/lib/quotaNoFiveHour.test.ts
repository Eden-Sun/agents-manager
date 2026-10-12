/**
 * codex 升級成沒有 5 小時窗的方案（使用者 2026-10-12）：快照只剩週窗，或 5h 只是沿用的舊讀數時，
 * 要認成「5h 無上限」；plus（兩個窗一起來）與其他 kind 不受影響。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import type { KindQuota, QuotaWindow } from '../api/types'
import { noFiveHourCap, projectNoFiveHour } from './quotaNoFiveHour'

const NEW = '2026-10-12T13:55:46.094Z'
const OLD = '2026-10-12T13:48:07.979Z'
const win = (used_pct: number, observed_at: string): QuotaWindow => ({ used_pct, resets_at: null, observed_at, low: false, critical: used_pct >= 95 }) as QuotaWindow
const quota = (over: Partial<KindQuota>): KindQuota => ({
  five_hour: null,
  seven_day: win(0, NEW),
  fable: null,
  reset_credits: null,
  limit_hit: null,
  plan: 'prolite',
  updated_at: NEW,
  stale: false,
  host: 'local',
  ...over,
})
const hit = { message: 'You’ve hit your usage limit', until: '2026-10-12T16:29:00.000Z', at: '2026-10-12T13:51:36.675Z', bucket: null }

test('只有週窗的 codex 是 5h 無上限', () => {
  assert.equal(noFiveHourCap('codex', quota({})), true)
})

test('週窗是新的、5h 是沿用的舊讀數：一樣是無上限，舊 5h 與升級前的撞上限不畫', () => {
  const q = quota({ five_hour: win(100, OLD), limit_hit: hit })
  assert.equal(noFiveHourCap('codex', q), true)
  const shown = projectNoFiveHour('codex', q)!
  assert.equal(shown.five_hour, null)
  assert.equal(shown.limit_hit, null)
  assert.equal(shown.seven_day?.used_pct, 0)
})

test('兩個窗都是這次帶進來的（plus）：不是無上限，原樣顯示', () => {
  const q = quota({ five_hour: win(40, NEW), plan: 'plus' })
  assert.equal(noFiveHourCap('codex', q), false)
  assert.equal(projectNoFiveHour('codex', q), q)
})

test('週窗真的用完時，撞上限照樣顯示', () => {
  const q = quota({ seven_day: win(100, NEW), limit_hit: hit })
  assert.deepEqual(projectNoFiveHour('codex', q)!.limit_hit, hit)
})

test('這份讀數之後才撞的上限照樣顯示', () => {
  const later = { ...hit, at: '2026-10-12T14:30:00.000Z' }
  assert.deepEqual(projectNoFiveHour('codex', quota({ limit_hit: later }))!.limit_hit, later)
})

test('還沒有任何讀數、或不是 codex：不適用', () => {
  assert.equal(noFiveHourCap('codex', null), false)
  assert.equal(noFiveHourCap('codex', quota({ seven_day: null })), false)
  assert.equal(noFiveHourCap('claude', quota({})), false)
  assert.equal(noFiveHourCap('grok', quota({})), false)
})

test('側欄：沿用的舊 5h（critical）不再讓 codex 的 bot 整列反灰、也不掛紅色額度徽章', async () => {
  const { botQuotaWarning, botQuotaLevel } = await import('../store/store')
  const stale = { codex: quota({ five_hour: { ...win(100, OLD), low: true, critical: true }, limit_hit: hit }) }
  assert.equal(botQuotaWarning(stale, 'codex', null), null)
  assert.equal(botQuotaLevel(stale, 'codex', null), null)
  // plus：5h 真的用完時照舊警告
  const real = { codex: quota({ five_hour: { ...win(100, NEW), low: true, critical: true }, plan: 'plus' }) }
  assert.equal(botQuotaWarning(real, 'codex', null)?.window, '5h')
})
