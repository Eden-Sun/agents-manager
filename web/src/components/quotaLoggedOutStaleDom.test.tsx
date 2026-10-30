/**
 * 登出了但還留著舊讀數的身份（2026-10-30 本機 cc1：10-25 登出後額度格一直灰著）：明細照樣要有登入入口。
 * 以前登入鈕只在「完全沒有讀數」時才畫。mock 的 cc1 正好是未登入＋有讀數。真的掛 `QuotaStrip` 進 happy-dom。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

virtualMockTime()
afterEach(unmountAll)
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const rowOf = (name: string) =>
  [...document.querySelectorAll<HTMLElement>('.quota-pop-row')].find((r) => r.querySelector('.quota-name')?.textContent === `claude · ${name}`) ?? null
const loginBtn = (row: HTMLElement) => [...row.querySelectorAll('button')].find((b) => b.textContent?.includes('開終端登入'))

test('未登入但有舊讀數的身份：明細寫明是舊讀數並給登入鈕；已登入的沒有', { timeout: 30_000 }, async () => {
  resetStoreForTest()
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await mount(<QuotaStrip />)
  await until(() => document.querySelector('.quota-bars-open') !== null, '額度列畫出來')
  await click(document.querySelector('.quota-bars-open')!)
  await until(() => rowOf('cc1') !== null, 'cc1 那列出現')
  const cc1 = rowOf('cc1')!
  assert.ok(cc1.querySelector('.quota-pop-line'), 'cc1 照樣畫出舊讀數')
  assert.match(cc1.textContent ?? '', /未登入，上面是登出前的舊讀數/)
  assert.ok(loginBtn(cc1), '未登入的 cc1 要有登入入口')
  const cc0 = rowOf('cc0')
  assert.ok(cc0, 'cc0 那列出現')
  assert.equal(loginBtn(cc0), undefined, '已登入的 cc0 沒有登入鈕')
})
