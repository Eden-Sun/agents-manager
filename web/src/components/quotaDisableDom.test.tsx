import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, mockApi, mount, act, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QUOTA_DISABLED_KEY, setQuotaDisabled } from '../store/quotaHide'
import { QuotaStrip } from './QuotaStrip'

const mock = sharedMock
const STRIP_KEY = 'local|claude|cc0'
const OTHER_KEY = 'local|claude|cc1'

const disk = (): Record<string, number | null> => JSON.parse(localStorage.getItem(QUOTA_DISABLED_KEY) ?? '{}')
const stripBox = () => document.querySelector<HTMLInputElement>('.quota-cell-toggle input[aria-label$="本機 · claude · cc0（底下的 Bot 先從側欄收起來，額度 reset 後自動回來）"], .quota-cell-toggle input[aria-label^="解除停用 本機 · claude · cc0"]')!
const popBox = () => document.querySelector<HTMLInputElement>('.quota-pop input[type=checkbox]')!

async function otherTabWrites(map: Record<string, number | null>) {
  localStorage.setItem(QUOTA_DISABLED_KEY, JSON.stringify(map))
  await act(async () => {
    window.dispatchEvent(new StorageEvent('storage', { key: QUOTA_DISABLED_KEY }))
  })
}

async function open() {
  mockApi(mock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  await mount(<QuotaStrip />)
  await until(() => stripBox() !== null, '額度條畫出來')
}

virtualMockTime()
afterEach(async () => {
  if (Object.keys(disk()).length || stripBox()?.checked) await otherTabWrites({})
  await unmountAll()
  localStorage.clear()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

it('條上勾停用：存進 localStorage（到期時間＝用完的那個窗口的 reset）、勾選與提示變成已停用；再勾一次解除', async () => {
  const quotas = (mock as unknown as { quota: Record<string, { five_hour: Record<string, unknown>; seven_day: Record<string, unknown> }> }).quota
  const q = quotas.claude
  const saved = { five: { ...q.five_hour }, seven: { ...q.seven_day } }
  q.seven_day = { ...q.seven_day, used_pct: 100, low: true, critical: true }
  q.five_hour = { ...q.five_hour, used_pct: 40, low: false, critical: false }
  try {
    await open()
    assert.equal(stripBox().checked, false)
    await click(stripBox())
    await until(() => STRIP_KEY in disk(), '寫進 localStorage')
    const until_ = disk()[STRIP_KEY]
    assert.equal(typeof until_, 'number', '有 reset 時間就自動解除')
    assert.ok((until_ as number) > Date.now(), '到期時間在未來')
    assert.equal(until_, Date.parse(q.seven_day.resets_at as string), '到期＝7d 的 reset')
    assert.ok((until_ as number) > Date.parse(q.five_hour.resets_at as string), '不是比較早的 5h')
  } finally {
    q.five_hour = saved.five
    q.seven_day = saved.seven
  }
  assert.equal(stripBox().checked, true)
  assert.match(stripBox().getAttribute('aria-label') ?? '', /^解除停用/)
  await click(stripBox())
  await until(() => !(STRIP_KEY in disk()), '解除後從 localStorage 拿掉')
  assert.equal(stripBox().checked, false)
})

it('popover 裡的勾選跟條上的是同一份：條上勾了，popover 那格也已勾、顯示「已停用」', async () => {
  await open()
  await click(document.querySelector('.quota-bars-open')!)
  await until(() => document.querySelector('.quota-pop') !== null, 'popover 開了')
  assert.equal(popBox().checked, false)
  await click(popBox())
  await until(() => STRIP_KEY in disk(), '從 popover 勾也寫進 localStorage')
  assert.equal(stripBox().checked, true, '條上同步')
  assert.equal(document.querySelector('.quota-pop .quota-disable-note')!.textContent, '已停用')
})

it('#755 別的分頁勾了：storage 事件讓這一頁的勾選跟上；別的分頁解除了也跟上', async () => {
  await open()
  assert.equal(stripBox().checked, false)
  await otherTabWrites({ [STRIP_KEY]: Date.now() + 3_600_000 })
  await until(() => stripBox().checked, '別的分頁勾的在這一頁也勾上')
  await otherTabWrites({})
  await until(() => !stripBox().checked, '別的分頁解除，這一頁也解除')
})

it('#755 happy-dom 重建後仍接收別的分頁 storage 事件', async () => {
  await open()
  await act(async () => {
    localStorage.clear()
    setQuotaDisabled(STRIP_KEY, false, null)
  })
  await unmountAll()
  await teardownDom()
  await setupDom()

  await open()
  await otherTabWrites({ [STRIP_KEY]: Date.now() + 3_600_000 })
  await until(() => stripBox().checked, '重建 DOM 後別的分頁勾的在這一頁也勾上')
})

it('#755 這個分頁勾 cc0，不能洗掉別的分頁剛勾、這個分頁還沒收到事件的 cc1', async () => {
  await open()
  localStorage.setItem(QUOTA_DISABLED_KEY, JSON.stringify({ [OTHER_KEY]: null }))
  await click(stripBox())
  await until(() => STRIP_KEY in disk(), '寫進 localStorage')
  assert.ok(OTHER_KEY in disk(), '別的分頁的 cc1 還在，沒被整份覆寫洗掉')
})

it('已過期的停用在別的分頁寫進來時就被掃掉，不會讓這一頁顯示成停用', async () => {
  await open()
  await otherTabWrites({ [STRIP_KEY]: Date.now() - 1000 })
  assert.equal(stripBox().checked, false)
})
