/**
 * codex 的方案沒有 5 小時窗（使用者 2026-10-12）：桌機量表的 5h 那一格寫「無上限」，
 * 不畫 daemon 沿用下來的舊 5h（「5h 舊 … 0%」）也不畫升級前的撞上限。真的掛 `QuotaStrip` 進 happy-dom。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

type MockQuota = { quota: Record<string, Record<string, unknown> | undefined> }
const mock = sharedMock as unknown as MockQuota
const original = mock.quota.codex
const NEW = '2026-10-12T13:55:46.094Z'
const OLD = '2026-10-12T13:48:07.979Z'
const cell = () => document.querySelector<HTMLElement>('.quota-hp.codex')

async function openWith(codex: Record<string, unknown>) {
  mock.quota.codex = { ...(original ?? {}), fable: null, reset_credits: null, stale: false, host: 'local', updated_at: NEW, ...codex }
  mockApi(sharedMock)
  resetStoreForTest()
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await mount(<QuotaStrip />)
  await until(() => cell() !== null, 'codex 格畫出來')
  return cell()!
}

virtualMockTime()
afterEach(async () => {
  mock.quota.codex = original
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const week = { used_pct: 4, resets_at: null, observed_at: NEW, low: false, critical: false }

test('沿用的舊 5h＋升級前的撞上限：畫成「5h 無上限」，舊讀數與橫幅都不見', { timeout: 30_000 }, async () => {
  const el = await openWith({
    plan: 'prolite',
    five_hour: { used_pct: 100, resets_at: '2026-10-12T16:29:23.000Z', observed_at: OLD, low: true, critical: true },
    seven_day: week,
    limit_hit: { message: 'You’ve hit your usage limit', until: '2026-10-12T16:29:00.000Z', at: '2026-10-12T13:51:36.675Z', bucket: null },
  })
  assert.equal(el.querySelector('.quota-no-cap')?.textContent, '無上限')
  assert.equal(el.querySelector('.quota-window-stale'), null, '不該再掛「舊」')
  const names = [...el.querySelectorAll('.quota-window-name')].map((n) => n.textContent)
  assert.deepEqual(names, ['5h', '7d'])
  assert.equal(el.querySelectorAll('.quota-bar').length, 1, '只有週窗畫條')
})

test('plus：兩個窗都是新的，照舊畫兩條、沒有「無上限」', { timeout: 30_000 }, async () => {
  const el = await openWith({ plan: 'plus', five_hour: { used_pct: 40, resets_at: null, observed_at: NEW, low: false, critical: false }, seven_day: week, limit_hit: null })
  assert.equal(el.querySelector('.quota-no-cap'), null)
  assert.equal(el.querySelectorAll('.quota-bar').length, 2)
})
