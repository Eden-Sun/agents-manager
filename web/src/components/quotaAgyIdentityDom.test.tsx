/**
 * 額度列的 agy 身份：每個 `kind = "agy"` 身份（自己的 HOME）一格，跟預設帳號的 agy 格並排，標身份名（`agy · ag1`）。
 * 還沒有讀數的身份也要列出來（顯示「背景查詢中」），不能像以前只認裸 `agy` 而整個消失。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import type { Identity } from '../api/types'
import { QuotaStrip } from './QuotaStrip'

virtualMockTime()
afterEach(async () => {
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const agyIdent = (name: string): Identity => ({ name, kind: 'agy', env: { HOME: `$HOME/.agy-${name}` }, args: [] })
const win = { used_pct: 10, low: false, critical: false, resets_at: '2026-11-20T00:00:00.000Z', observed_at: '2026-11-15T00:00:00.000Z' }
const agyQuota = (used: number) => ({ five_hour: { ...win, used_pct: used }, seven_day: { ...win, used_pct: used }, fable: null, reset_credits: null, limit_hit: null, plan: null, account: null, host: 'local', source: 'agy-usage', stale: false, updated_at: '2026-11-15T00:00:00.000Z' })

test('agy 身份每個一格、標身份名；還沒有讀數的身份也列出來', async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  useStore.setState({ identities: [agyIdent('ag1'), agyIdent('ag2')] } as never)
  await useStore.getState().loadQuota()
  // 預設帳號有讀數、ag1 有自己的讀數、ag2 還沒探到。
  useStore.setState({ quota: { agy: agyQuota(40), 'agy:ag1': agyQuota(25) } } as never)
  await mount(<QuotaStrip />)
  await until(() => document.querySelectorAll('.quota-hp.agy').length >= 3, '預設 agy 加兩個身份的格畫出來')
  const names = [...document.querySelectorAll<HTMLElement>('.quota-hp.agy')].map((el) => el.querySelector('.quota-identity')?.textContent ?? '')
  assert.ok(names.includes('ag1') && names.includes('ag2'), JSON.stringify(names))
  const ag1 = [...document.querySelectorAll<HTMLElement>('.quota-hp.agy')].find((el) => el.querySelector('.quota-identity')?.textContent === 'ag1')
  const title = ag1?.querySelector('.quota-bars-open')?.getAttribute('aria-label') ?? ''
  assert.ok(title.includes('agy · ag1'), title || 'no ag1 cell')
})
