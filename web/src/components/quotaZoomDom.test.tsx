/**
 * 剩量和重置刻度都在 10% 以下時，量表條改用 0–10% 的刻度（×10）；倒數那一列也畫條（#1784，2026-10-16 使用者）。
 * 真的掛 `QuotaStrip` 進 happy-dom，量填色寬、刻度位置與 ×10 記號。
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

const WEEK_MS = 7 * 24 * 3600 * 1000

/** 週窗：剩 `remaining`%、重置刻度落在窗口的 `markPct`%（`resets_at` = 現在 + 窗口長 × markPct%）。 */
async function mountWeek(remaining: number, markPct: number) {
  resetStoreForTest()
  const resets_at = new Date(Date.now() + (WEEK_MS * markPct) / 100).toISOString()
  useStore.setState({
    quota: {
      claude: {
        five_hour: null,
        seven_day: { used_pct: 100 - remaining, low: remaining < 30, critical: remaining < 10, resets_at, observed_at: null },
        fable: null,
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

const widthOf = (el: Element) => parseFloat(el.querySelector<HTMLElement>('.quota-bar-fill')!.style.width)
const markOf = (el: Element) => parseFloat(el.querySelector<HTMLElement>('.quota-bar-mark')!.style.left)

test('剩 8%、刻度 1.8%：填色寬 80%、刻度 18%、有 ×10（倒數列也一樣）', async () => {
  const cell = await mountWeek(8, 1.8)
  const wrap = cell.querySelector('.quota-bar-wrap')!
  assert.ok(wrap.classList.contains('zoomed'))
  assert.equal(widthOf(cell), 80)
  assert.ok(Math.abs(markOf(cell) - 18) < 0.1, `刻度該在 18%，實際 ${markOf(cell)}`)
  const zoom = wrap.querySelector('.quota-bar-zoom')
  assert.equal(zoom?.textContent, '×10')
  assert.equal(zoom?.getAttribute('title'), '刻度放大：整條＝10%')
})

test('倒數列有條也有倒數字，順序是條、倒數、百分比', async () => {
  const cell = await mountWeek(8, 1.8)
  const row = cell.querySelector('.quota-bar-row.countdown')!
  assert.ok(row, '剩 8%、三小時內重置的週窗是倒數列')
  const order = [...row.children].map((c) => (c.classList.contains('quota-bar-wrap') ? 'bar' : c.classList.contains('quota-reset-at') ? 'reset' : c.classList.contains('quota-bar-pct') ? 'pct' : '?'))
  assert.deepEqual(order, ['bar', 'reset', 'pct'])
  assert.match(row.querySelector('.quota-reset-at')!.textContent ?? '', /\d/)
  assert.equal(row.querySelector('.quota-bar-pct')!.textContent, '8%')
})

test('剩 50%：不縮放，寬度照剩量、沒有 ×10', async () => {
  const cell = await mountWeek(50, 40)
  assert.equal(cell.querySelector('.quota-bar-wrap.zoomed'), null)
  assert.equal(cell.querySelector('.quota-bar-zoom'), null)
  assert.equal(widthOf(cell), 50)
  assert.ok(Math.abs(markOf(cell) - 40) < 0.1)
})

test('剩 8% 但刻度 40%：不縮放（刻度放不進 0–10%）', async () => {
  const cell = await mountWeek(8, 40)
  assert.equal(cell.querySelector('.quota-bar-wrap.zoomed'), null)
  assert.equal(cell.querySelector('.quota-bar-zoom'), null)
  assert.equal(widthOf(cell), 8)
  assert.ok(Math.abs(markOf(cell) - 40) < 0.1)
})

test('popover 的同一條也縮放', async () => {
  const cell = await mountWeek(8, 1.8)
  await click(cell.querySelector('.quota-bars-open')!)
  const pop = document.querySelector<HTMLElement>('.quota-pop')!
  const wrap = pop.querySelector('.quota-bar-wrap')!
  assert.ok(wrap.classList.contains('zoomed'))
  assert.equal(wrap.querySelector('.quota-bar-zoom')?.textContent, '×10')
  assert.equal(widthOf(pop), 80)
})
