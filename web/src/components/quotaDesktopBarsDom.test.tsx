/** 窄桌機額度列換到自己的新行時，仍畫出每個窗口（happy-dom）。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { KindQuota, QuotaWindow } from '../api/types'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

let restoreLayout: () => void = () => {}

before(setupDom)
afterEach(async () => {
  await unmountAll()
  restoreLayout()
  restoreLayout = () => {}
})
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

function narrowDesktopLayout(): void {
  const oldMatchMedia = window.matchMedia
  const oldClientWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'clientWidth')
  const oldBounds = HTMLElement.prototype.getBoundingClientRect
  const oldRects = HTMLElement.prototype.getClientRects
  window.matchMedia = ((query: string) => ({
    matches: false,
    media: query,
    addEventListener() {},
    removeEventListener() {},
  })) as unknown as typeof window.matchMedia
  Object.defineProperty(HTMLElement.prototype, 'clientWidth', {
    configurable: true,
    get() {
      return this.classList.contains('main-head') ? 420 : 0
    },
  })
  HTMLElement.prototype.getBoundingClientRect = function () {
    let left = 0
    let width = 24
    if (this.classList.contains('main-title')) width = 260
    else if (this.classList.contains('tabs')) width = 120
    else if (this.classList.contains('quota-hp')) {
      const cells = this.parentElement ? [...this.parentElement.children].filter((el) => el.classList.contains('quota-hp')) : []
      left = 30 + Math.max(0, cells.indexOf(this)) * 130
      width = 130
    } else if (this.classList.contains('quota-open') || this.classList.contains('quota-strip')) width = 420
    else if (this.classList.contains('quota-update')) width = 24
    return new DOMRect(left, 0, width, 36)
  }
  HTMLElement.prototype.getClientRects = function (): DOMRectList {
    const rects = [this.getBoundingClientRect()]
    return Object.assign(rects, { item: (index: number) => rects[index] ?? null }) as unknown as DOMRectList
  }
  restoreLayout = () => {
    window.matchMedia = oldMatchMedia
    if (oldClientWidth) Object.defineProperty(HTMLElement.prototype, 'clientWidth', oldClientWidth)
    else delete (HTMLElement.prototype as unknown as { clientWidth?: number }).clientWidth
    HTMLElement.prototype.getBoundingClientRect = oldBounds
    HTMLElement.prototype.getClientRects = oldRects
  }
}

const hoursAhead = (hours: number) => new Date(Date.now() + hours * 3_600_000).toISOString()
const win = (used: number, resetHours: number): QuotaWindow => ({
  used_pct: used,
  resets_at: hoursAhead(resetHours),
  observed_at: null,
  low: false,
  critical: false,
})

test('窄桌機把額度列換到新行，claude 的 5h、7d、F 仍全部顯示', async () => {
  narrowDesktopLayout()
  const quota: KindQuota = {
    five_hour: win(20, 4),
    seven_day: win(35, 120),
    fable: win(42, 120),
    reset_credits: null,
    limit_hit: null,
    plan: 'Max',
    updated_at: new Date().toISOString(),
    stale: false,
    host: 'local',
  }
  useStore.setState((s) => ({ ...s, quota: { claude: quota }, identities: [], disabledIdentities: [] }) as never)

  await mount(
    <div className="main-head">
      <div className="main-title">Bot</div>
      <span className="spacer" />
      <QuotaStrip />
      <div className="tabs" />
    </div>,
  )
  await act(async () => {})

  const strip = document.querySelector<HTMLElement>('.quota-strip')!
  assert.ok(strip.classList.contains('stacked'), '空間不足時額度列進入獨立新行')
  assert.equal(strip.querySelector('.quota-open.collapsed'), null, '桌機不使用單窗口收合')
  const cell = strip.querySelector<HTMLElement>('.quota-hp.claude')!
  const names = [...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent?.trim())
  assert.deepEqual(names, ['5h', '7d', 'F'])
})

test('桌機只有 F 窗口資料時不以無資料 placeholder 蓋掉 F', async () => {
  narrowDesktopLayout()
  const quota: KindQuota = {
    five_hour: null,
    seven_day: null,
    fable: win(42, 120),
    reset_credits: null,
    limit_hit: null,
    plan: 'Max',
    updated_at: new Date().toISOString(),
    stale: false,
    host: 'local',
  }
  useStore.setState((s) => ({ ...s, quota: { claude: quota }, identities: [], disabledIdentities: [] }) as never)

  await mount(
    <div className="main-head">
      <div className="main-title">Bot</div>
      <span className="spacer" />
      <QuotaStrip />
      <div className="tabs" />
    </div>,
  )
  await act(async () => {})

  const names = [...document.querySelectorAll('.quota-hp.claude .quota-window-name')].map((el) => el.textContent?.trim())
  assert.deepEqual(names, ['F'])
})
