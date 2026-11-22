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
const agyQuota = { five_hour: win, seven_day: win, fable: null, reset_credits: null, limit_hit: null, plan: null, account: null, host: 'local', source: 'agy' }

const ag2Cell = () =>
  [...document.querySelectorAll<HTMLElement>('.quota-hp.agy')].find((el) => el.querySelector('.quota-identity')?.textContent === 'ag2') ?? null

async function mountWith(extra: Record<string, unknown>) {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  useStore.setState({ identities: [agyIdent('ag1'), agyIdent('ag2')] } as never)
  await useStore.getState().loadQuota()
  useStore.setState({ quota: { agy: agyQuota, 'agy:ag1': agyQuota }, ...extra } as never)
  await mount(<QuotaStrip />)
  await until(() => ag2Cell() !== null, 'ag2 那格畫出來')
  return ag2Cell()!
}

test('agy 身份還沒有讀數：格子顯示「查詢中…」，不會只剩窗口名 5h', async () => {
  const cell = await mountWith({})
  const text = cell.textContent ?? ''
  assert.match(text, /查詢中…/, text)
  assert.equal(cell.querySelector('.quota-window'), null, '沒有讀數時不畫窗口列')
  assert.equal(cell.querySelector('.quota-placeholder')?.textContent, '查詢中…')
})

test('agy 身份未登入且沒有讀數：格子顯示「未登入」', async () => {
  const cell = await mountWith({
    localIdentityStatus: { ag2: { logged_in: false, config_dir: null, kind: 'agy' } },
  })
  assert.equal(cell.querySelector('.quota-placeholder')?.textContent, '未登入')
  assert.doesNotMatch(cell.textContent ?? '', /查詢中/)
})

test('有讀數的身份不顯示佔位', async () => {
  const cell = await mountWith({})
  const ag1 = [...document.querySelectorAll<HTMLElement>('.quota-hp.agy')].find((el) => el.querySelector('.quota-identity')?.textContent === 'ag1')!
  assert.equal(ag1.querySelector('.quota-placeholder'), null)
  assert.ok(cell)
})
