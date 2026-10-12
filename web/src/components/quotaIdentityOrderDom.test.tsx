/**
 * 額度列的 claude 身分順序：cc0 → cc1 → 其他（照名字的自然順序）。`cc10` 要排在 `cc2` 後面，不是字典序的前面。
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

const ident = (name: string): Identity => ({ name, kind: 'claude', env: {}, args: [] })

test('claude 身分照自然順序排：cc0、cc1、cc2、cc10（不是字典序的 cc10 排在 cc2 前）', async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  useStore.setState({ identities: ['cc10', 'cc2', 'cc0', 'cc1'].map(ident) } as never)
  await useStore.getState().loadQuota()
  await mount(<QuotaStrip />)
  await until(() => document.querySelectorAll('.quota-hp.claude').length >= 4, '四個 claude 身分的格畫出來')
  const order = [...document.querySelectorAll<HTMLElement>('.quota-hp.claude')].map((el) => el.querySelector('.quota-identity')?.textContent)
  assert.deepEqual(order.slice(0, 4), ['cc0', 'cc1', 'cc2', 'cc10'], JSON.stringify(order))
})
