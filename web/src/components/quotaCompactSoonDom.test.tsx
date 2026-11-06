/** 手機每個身份只畫 7d，倒數也只看週窗；真的掛 QuotaStrip 進 happy-dom。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

type MockQuota = { quota: Record<string, Record<string, unknown> | undefined> }
const mock = sharedMock as unknown as MockQuota
const originalQuota = { ...mock.quota }

function phoneWidth(): () => void {
  const original = window.matchMedia
  window.matchMedia = ((q: string) => ({ matches: q.includes('max-width: 640px'), media: q, addEventListener() {}, removeEventListener() {} })) as unknown as typeof window.matchMedia
  return () => (window.matchMedia = original)
}

virtualMockTime()
afterEach(async () => {
  Object.assign(mock.quota, originalQuota)
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

async function mountPhoneQuota(kind: string, values: Record<string, unknown>) {
  resetStoreForTest()
  mock.quota[kind] = { ...mock.quota[kind], ...values }
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  if (kind === 'agy') useStore.setState((s) => ({ localTools: { ...s.localTools, agy: { ...s.localTools.agy, logged_in: true } } }))
  await mount(<QuotaStrip />)
  await until(() => document.querySelector(`.quota-hp.${kind} .quota-mobile-bars`) !== null, '手機量表畫出來')
  return document.querySelector<HTMLElement>(`.quota-hp.${kind}`)!
}

const windowIn = (used: number, hours: number, low = false, critical = false) => ({
  used_pct: used, low, critical, resets_at: new Date(Date.now() + hours * 3_600_000).toISOString(), observed_at: null,
})


for (const kind of ['claude', 'codex', 'agy', 'grok']) {
  test(`手機：${kind} 只畫一個 7d，保留百分比與週重置針，不被更急的 5h／F 取代`, async () => {
    const restore = phoneWidth()
    try {
      const cell = await mountPhoneQuota(kind, {
        five_hour: windowIn(100, 0.4, true, true), seven_day: windowIn(10, 84), fable: windowIn(99, 1, true, true), limit_hit: null,
      })
      const rows = [...cell.querySelectorAll<HTMLElement>('.quota-window')]
      assert.deepEqual(rows.map((row) => row.querySelector('.quota-window-name')?.textContent), ['7d'])
      assert.equal(rows[0].querySelector('.quota-bar-pct')?.textContent, '90%')
      assert.equal(rows[0].querySelector<HTMLElement>('.quota-bar-fill')?.style.width, '90%')
      const mark = rows[0].querySelector<HTMLElement>('.quota-bar-mark')!
      assert.ok(Math.abs(parseFloat(mark.style.left) - 50) < 0.1)
      assert.match(mark.title, /7d.*重置/)
      assert.ok(rows[0].querySelector('.quota-bar.ok'), '不被其他窗口警告染色')
      assert.equal(cell.querySelector('.quota-reset-line'), null, '不顯示 5h／F 倒數')
    } finally {
      restore()
    }
  })
}

test('手機：grok 只有週窗也作為 7d 顯示', async () => {
  const restore = phoneWidth()
  try {
    const cell = await mountPhoneQuota('grok', { five_hour: null, seven_day: windowIn(25, 84), fable: null, limit_hit: null })
    assert.deepEqual([...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent), ['7d'])
    assert.equal(cell.querySelector('.quota-bar-pct')?.textContent, '75%')
    assert.equal(cell.querySelectorAll('.quota-bar-mark').length, 1)
  } finally {
    restore()
  }
})

for (const kind of ['claude', 'codex', 'agy', 'grok']) {
  test(`手機：${kind} 缺 7d 時顯示無資料，不用 5h／F 代替`, async () => {
    const restore = phoneWidth()
    try {
      const cell = await mountPhoneQuota(kind, {
        five_hour: windowIn(100, 0.4, true, true), seven_day: null, fable: windowIn(99, 1, true, true), limit_hit: null,
      })
      assert.deepEqual([...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent), ['7d'])
      assert.equal(cell.querySelector('.quota-bar-pct')?.textContent, '無資料')
      assert.equal(cell.querySelector('.quota-bar'), null)
      assert.equal(cell.querySelector('.quota-bar-mark'), null)
      assert.equal(cell.querySelector('.quota-reset-line'), null)
    } finally {
      restore()
    }
  })
}

test('手機：撞限只顯示 7d 百分比與一行週倒數，維持低額度放大條', async () => {
  const restore = phoneWidth()
  try {
    const cell = await mountPhoneQuota('claude', {
      five_hour: windowIn(100, 0.4, true, true), seven_day: windowIn(100, 6, true, true), fable: null,
      limit_hit: { message: 'hit', until: null, at: new Date().toISOString(), bucket: 'seven_day' },
    })
    assert.ok(cell.classList.contains('quota-blocked'))
    assert.deepEqual([...cell.querySelectorAll('.quota-bar-pct')].map((el) => el.textContent), ['0%'])
    assert.equal(cell.querySelectorAll('.quota-bar-wrap.zoomed').length, 1)
    assert.equal(cell.querySelector('.quota-bar-mark'), null, '放大刻度不混用黑針')
    assert.equal(cell.querySelectorAll('.quota-reset-line').length, 1)
    assert.match(cell.querySelector('.quota-reset-line')?.textContent ?? '', /^└\s*(5h59|6h00)$/)
    assert.match(cell.querySelector('.quota-reset-line')?.getAttribute('title') ?? '', /^7d.*重置$/)
    assert.deepEqual([...cell.querySelectorAll('.quota-window-name')].map((el) => el.textContent), ['7d'])
  } finally {
    restore()
  }
})
