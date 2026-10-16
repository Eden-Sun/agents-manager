/**
 * 剩量和重置刻度都在 10% 以下時，量表條改畫 0–10% 刻度的連續條、兩端各一根豎線（#1787，2026-10-16 第三版，取代 #1785 的 10 格點陣）；
 * 倒數模式的列不在列內寫倒數，統一在所有窗口下方另起一行（`└ 3h05`）。真的掛 `QuotaStrip` 進 happy-dom。
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

const fillOf = (el: Element) => parseFloat(el.querySelector<HTMLElement>('.quota-bar-fill')!.style.width)
const ends = (el: Element) => el.querySelectorAll('.quota-bar-end').length
const line = (el: Element) => el.querySelector('.quota-reset-line')

test('剩 3%：填色 30%、兩端各一根豎線，沒有格子、黑針、×10', async () => {
  const cell = await mountWeek(3, 1.8)
  assert.equal(fillOf(cell), 30)
  assert.equal(ends(cell), 2)
  assert.equal(cell.querySelector('.quota-cells'), null)
  assert.equal(cell.querySelector('.quota-cell'), null)
  assert.equal(cell.querySelector('.quota-bar-mark'), null)
  assert.equal(cell.querySelector('.quota-bar-zoom'), null)
  assert.ok(!/×10/.test(cell.textContent ?? ''))
  assert.equal(cell.querySelector('.quota-bar-wrap')!.getAttribute('title'), '剩 3%（條的刻度是 0–10%）')
  assert.equal(cell.querySelector('.quota-bar-pct')!.textContent, '3%')
})

test('剩 0%：填色 0、兩端豎線還在', async () => {
  const cell = await mountWeek(0, 1.8)
  assert.equal(fillOf(cell), 0)
  assert.equal(ends(cell), 2)
  assert.equal(cell.querySelector('.quota-bar-pct')!.textContent, '0%')
})

test('剩 50%：普通條，沒有豎線，有黑針', async () => {
  const cell = await mountWeek(50, 40)
  assert.equal(ends(cell), 0)
  assert.equal(fillOf(cell), 50)
  assert.ok(Math.abs(parseFloat(cell.querySelector<HTMLElement>('.quota-bar-mark')!.style.left) - 40) < 0.1)
})

test('剩 8% 但刻度 40%：不畫刻度條（刻度要靠一比一的條看）', async () => {
  const cell = await mountWeek(8, 40)
  assert.equal(ends(cell), 0)
  assert.equal(fillOf(cell), 8)
})

test('倒數模式的列：列內沒有倒數文字，只有條和百分比；倒數在底下一行', async () => {
  const cell = await mountWeek(3, 1.8)
  const row = cell.querySelector('.quota-bar-row')!
  assert.equal(row.querySelector('.quota-reset-at'), null)
  assert.deepEqual([...row.children].map((c) => (c.classList.contains('quota-bar-wrap') ? 'bar' : c.classList.contains('quota-bar-pct') ? 'pct' : '?')), ['bar', 'pct'])
  assert.equal(cell.querySelectorAll('.quota-reset-at').length, 0)
  assert.match(line(cell)!.textContent ?? '', /^└ \d+h\d\d$/)
  assert.ok(!/重置/.test(line(cell)!.textContent ?? ''))
})

test('7d 與 F 同一刻重置：底下一行只寫一個時間，不寫「重置」', async () => {
  const cell = await mountQuota({ remaining: 3, inMs: 3 * HOUR_MS }, { remaining: 0, inMs: 3 * HOUR_MS + 30_000 })
  const lines = cell.querySelectorAll('.quota-reset-line')
  assert.equal(lines.length, 1)
  assert.match(lines[0]!.textContent ?? '', /^└ \d+h\d\d$/)
  assert.ok(!/重置|7d|F/.test(lines[0]!.textContent ?? ''))
  const rows = cell.querySelectorAll('.quota-bar-row')
  assert.equal(rows.length, 2)
  assert.deepEqual([...rows].map((r) => r.querySelector('.quota-bar-pct')!.textContent), ['3%', '0%'])
  assert.deepEqual([...rows].map((r) => fillOf(r)), [30, 0])
  for (const r of rows) assert.equal(r.querySelector('.quota-reset-at'), null)
})

test('7d 與 F 不同一刻重置（差 5 分鐘）：底下一行兩個時間分別標 7d、F', async () => {
  const cell = await mountQuota({ remaining: 3, inMs: 3 * HOUR_MS }, { remaining: 0, inMs: 3 * HOUR_MS + 5 * 60_000 })
  assert.equal(cell.querySelectorAll('.quota-reset-line').length, 1)
  assert.match(line(cell)!.textContent ?? '', /^└ 7d \d+h\d\d · F \d+h\d\d$/)
  assert.equal(cell.querySelectorAll('.quota-reset-at').length, 0)
})

test('沒有任何列在倒數：不出底下那一行', async () => {
  const cell = await mountWeek(50, 40)
  assert.equal(line(cell), null)
})

test('7d 在倒數、F 還有很多：底下一行只寫 7d 的時間', async () => {
  const cell = await mountQuota({ remaining: 3, inMs: 3 * HOUR_MS }, { remaining: 60, inMs: 3 * HOUR_MS })
  assert.match(line(cell)!.textContent ?? '', /^└ \d+h\d\d$/)
  assert.equal(cell.querySelectorAll('.quota-reset-line').length, 1)
})

test('popover 的低額度列用同一種條，不加底下那一行', async () => {
  const cell = await mountWeek(3, 1.8)
  await click(cell.querySelector('.quota-bars-open')!)
  const pop = document.querySelector<HTMLElement>('.quota-pop')!
  assert.equal(fillOf(pop), 30)
  assert.equal(ends(pop), 2)
  assert.equal(pop.querySelector('.quota-reset-line'), null)
  assert.ok(pop.querySelector('.quota-bar-pct')!.classList.contains('has-unit'), 'popover 的 CSS 會補 %，已帶 % 的不能再補一次')
  assert.equal(pop.querySelector('.quota-bar-zoom'), null)
})
