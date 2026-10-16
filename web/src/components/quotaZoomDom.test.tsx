/**
 * 剩量和重置刻度都在 10% 以下時，量表條改畫 10 個小格、每格 1%（#1785，2026-10-16 第二版，取代 #1784 的 ×10 連續條）；
 * 7d 與 F 同一刻重置時，倒數只寫一次。真的掛 `QuotaStrip` 進 happy-dom。
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

const HOUR_MS = 3600 * 1000
const WEEK_MS = 7 * 24 * HOUR_MS

interface Win {
  remaining: number
  /** 距離重置還有幾毫秒。 */
  inMs: number
}

function win({ remaining, inMs }: Win) {
  return {
    used_pct: 100 - remaining,
    low: remaining < 30,
    critical: remaining < 10,
    resets_at: new Date(Date.now() + inMs).toISOString(),
    observed_at: null,
  }
}

async function mountQuota(seven: Win, fable: Win | null = null) {
  resetStoreForTest()
  useStore.setState({
    quota: {
      claude: {
        five_hour: null,
        seven_day: win(seven),
        fable: fable ? win(fable) : null,
        reset_credits: null,
        limit_hit: null,
        plan: null,
        updated_at: new Date().toISOString(),
        stale: false,
        host: 'local',
      },
    },
    identities: [],
  } as never)
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-hp.claude') !== null, 'Claude 額度格畫出來')
  return document.querySelector<HTMLElement>('.quota-hp.claude')!
}

/** 週窗：剩 `remaining`%、重置刻度落在窗口的 `markPct`%（重置在 窗口長 × markPct% 之後）。 */
const mountWeek = (remaining: number, markPct: number) => mountQuota({ remaining, inMs: (WEEK_MS * markPct) / 100 })

const solid = (el: Element) => el.querySelectorAll('.quota-cells .quota-cell.on').length
const hollow = (el: Element) => el.querySelectorAll('.quota-cells .quota-cell:not(.on)').length

test('剩 3%：3 格實心、7 格空心，沒有連續條也沒有 ×10', async () => {
  const cell = await mountWeek(3, 1.8)
  assert.equal(solid(cell), 3)
  assert.equal(hollow(cell), 7)
  assert.equal(cell.querySelector('.quota-bar'), null)
  assert.equal(cell.querySelector('.quota-bar-zoom'), null)
  assert.equal(cell.querySelector('.quota-bar-mark'), null)
  assert.ok(!/×10/.test(cell.textContent ?? ''))
  const cells = cell.querySelector('.quota-cells')!
  assert.equal(cells.getAttribute('aria-label'), '剩 3%（每格 1%）')
  assert.equal(cells.getAttribute('title'), '剩 3%（每格 1%）')
})

test('剩 0%：10 格全空心', async () => {
  const cell = await mountWeek(0, 1.8)
  assert.equal(solid(cell), 0)
  assert.equal(hollow(cell), 10)
})

test('剩 50%：不畫格子，照原本的連續條', async () => {
  const cell = await mountWeek(50, 40)
  assert.equal(cell.querySelector('.quota-cells'), null)
  assert.equal(parseFloat(cell.querySelector<HTMLElement>('.quota-bar-fill')!.style.width), 50)
  assert.ok(Math.abs(parseFloat(cell.querySelector<HTMLElement>('.quota-bar-mark')!.style.left) - 40) < 0.1)
})

test('剩 8% 但刻度 40%：不畫格子（刻度要靠連續條看）', async () => {
  const cell = await mountWeek(8, 40)
  assert.equal(cell.querySelector('.quota-cells'), null)
  assert.equal(parseFloat(cell.querySelector<HTMLElement>('.quota-bar-fill')!.style.width), 8)
})

test('只有一列在倒數：照舊，格子、倒數、百分比三者都在', async () => {
  const cell = await mountWeek(3, 1.8)
  const row = cell.querySelector('.quota-bar-row.countdown')!
  assert.ok(row, '剩 3%、三小時內重置的週窗是倒數列')
  const order = [...row.children].map((c) => (c.classList.contains('quota-cells') ? 'cells' : c.classList.contains('quota-reset-at') ? 'reset' : c.classList.contains('quota-bar-pct') ? 'pct' : '?'))
  assert.deepEqual(order, ['cells', 'reset', 'pct'])
  assert.equal(row.querySelector('.quota-bar-pct')!.textContent, '3%')
  assert.equal(cell.querySelector('.quota-shared'), null)
})

test('7d 與 F 同一刻重置：倒數只出現一次，兩列只剩百分比', async () => {
  const cell = await mountQuota({ remaining: 3, inMs: 3 * HOUR_MS }, { remaining: 0, inMs: 3 * HOUR_MS + 30_000 })
  const resets = cell.querySelectorAll('.quota-reset-at')
  assert.equal(resets.length, 1, '同一刻只寫一次倒數')
  assert.match(resets[0]!.textContent ?? '', /^\d+h\d\d$/)
  assert.ok(resets[0]!.closest('.quota-shared-reset'), '倒數掛在跨列的共用元素上')
  const rows = cell.querySelectorAll('.quota-shared .quota-bar-row')
  assert.equal(rows.length, 2)
  for (const r of rows) assert.ok(!r.classList.contains('countdown'))
  assert.deepEqual([...rows].map((r) => r.querySelector('.quota-bar-pct')!.textContent), ['3%', '0%'])
  assert.equal(solid(rows[0]!), 3)
  assert.equal(hollow(rows[1]!), 10)
})

test('7d 與 F 不同一刻重置（差 5 分鐘）：各寫各的倒數', async () => {
  const cell = await mountQuota({ remaining: 3, inMs: 3 * HOUR_MS }, { remaining: 0, inMs: 3 * HOUR_MS + 5 * 60_000 })
  assert.equal(cell.querySelectorAll('.quota-reset-at').length, 2)
  assert.equal(cell.querySelector('.quota-shared'), null)
})

test('7d 在倒數、F 還有很多：不合併', async () => {
  const cell = await mountQuota({ remaining: 3, inMs: 3 * HOUR_MS }, { remaining: 60, inMs: 3 * HOUR_MS })
  assert.equal(cell.querySelectorAll('.quota-reset-at').length, 1)
  assert.equal(cell.querySelector('.quota-shared'), null)
})

test('popover 的同一條也畫格子（不合併倒數）', async () => {
  const cell = await mountWeek(3, 1.8)
  await click(cell.querySelector('.quota-bars-open')!)
  const pop = document.querySelector<HTMLElement>('.quota-pop')!
  assert.equal(pop.querySelectorAll('.quota-cells .quota-cell.on').length, 3)
  assert.equal(pop.querySelectorAll('.quota-cells .quota-cell:not(.on)').length, 7)
  assert.equal(pop.querySelector('.quota-bar-zoom'), null)
})
