/**
 * codex 重置券到期日（2026-11-07 使用者）：彈窗每張券一列（標題、到期日、倒數），不到 48 小時標警示色；
 * logo 右上角有張數徽章（快到期警示色、tooltip 寫最近到期）；舊 daemon 沒 credits 時退回只寫張數。真的掛 `QuotaStrip` 進 happy-dom。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const HOUR = 3600_000
const at = (h: number) => new Date(Date.now() + h * HOUR).toISOString()
const credit = (id: string, h: number) => ({ id, title: 'Full reset', status: 'available', granted_at: null, expires_at: at(h) })

async function mountCodex(resetCredits: unknown) {
  resetStoreForTest()
  useStore.setState({
    quota: {
      codex: {
        five_hour: { used_pct: 20, resets_at: at(3), observed_at: null, low: false, critical: false },
        seven_day: { used_pct: 30, resets_at: at(100), observed_at: null, low: false, critical: false },
        fable: null,
        reset_credits: resetCredits,
        limit_hit: null,
        plan: 'plus',
        updated_at: new Date().toISOString(),
        stale: false,
        host: 'local',
      },
    },
    identities: [],
  } as never)
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-hp.codex') !== null, 'Codex 額度格畫出來')
  return document.querySelector<HTMLElement>('.quota-hp.codex')!
}

test('彈窗每張券一列：到期日＋倒數，最早的在前，不到 48 小時標警示', async () => {
  const cell = await mountCodex({ available: 2, title: 'Full reset', expires_at: at(30), credits: [credit('b', 24 * 24), credit('a', 30)] })
  await click(cell.querySelector('.quota-bars-open')!)
  const rows = [...document.querySelectorAll('.quota-pop .reset-credit-row')]
  assert.equal(rows.length, 2)
  assert.match(rows[0].textContent ?? '', /Full reset/)
  assert.match(rows[0].querySelector('.quota-credit-in')!.textContent ?? '', /^(29|30) 小時後$/)
  assert.ok(rows[0].classList.contains('urgent'), '30 小時內到期＝警示色')
  assert.match(rows[1].querySelector('.quota-credit-in')!.textContent ?? '', /^(23|24) 天後$/)
  assert.ok(!rows[1].classList.contains('urgent'))
})

test('logo 左上角的票券圖示＋小字寫可用張數，tooltip 寫最近到期日', async () => {
  const cell = await mountCodex({ available: 2, title: 'x', expires_at: at(24 * 24), credits: [credit('a', 24 * 24), credit('b', 24 * 30)] })
  const badge = cell.querySelector('.quota-kind .quota-credit-badge')!
  assert.equal(badge.textContent, '2')
  assert.ok(badge.querySelector('svg.quota-credit-icon'), '數字前有小票券圖示')
  const kind = cell.querySelector('.quota-kind')!
  assert.equal(kind.firstElementChild, badge, '徽章排在 logo 前面（左邊），不疊在 logo 上')
  assert.match(badge.getAttribute('title') ?? '', /2 張重置券可用.*(23|24) 天後/)
  assert.ok(!badge.classList.contains('urgent'))
})

test('最近一張不到 48 小時，徽章換警示色', async () => {
  const near = await mountCodex({ available: 1, title: 'x', expires_at: at(5), credits: [credit('a', 5)] })
  assert.ok(near.querySelector('.quota-credit-badge')!.classList.contains('urgent'))
})

test('其他 kind 沒有徽章', async () => {
  const cell = await mountCodex({ available: 1, title: 'x', expires_at: at(5), credits: [credit('a', 5)] })
  assert.equal(document.querySelectorAll('.quota-hp:not(.codex) .quota-credit-badge').length, 0)
  assert.ok(cell)
})

test('舊 daemon 沒有 credits：彈窗照舊只有張數，沒有逐張列，徽章仍寫張數', async () => {
  const cell = await mountCodex({ available: 1, title: 'Full reset', expires_at: at(24 * 10), credits: [] })
  await click(cell.querySelector('.quota-bars-open')!)
  assert.equal(document.querySelectorAll('.quota-pop .reset-credit-row').length, 0)
  assert.match(document.querySelector('.quota-pop .reset-credits')!.textContent ?? '', /1 張可用/)
  assert.equal(cell.querySelector('.quota-credit-badge')!.textContent, '1')
})

test('沒有重置券（或 0 張）就沒有徽章', async () => {
  const cell = await mountCodex(null)
  assert.equal(cell.querySelector('.quota-credit-badge'), null)
})
