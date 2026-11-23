import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, mockApi, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { sharedMock, virtualMockTime } from '../testing/sharedMock'
import { resetStoreForTest, useStore } from '../store/store'
import { QuotaStrip } from './QuotaStrip'

const rect = (width: number) => ({ x: 0, y: 0, left: 0, top: 0, right: width, bottom: 0, width, height: 0, toJSON() { return this } }) as DOMRect

function squeezeLayout(onRead?: () => void): () => void {
  const proto = HTMLElement.prototype as unknown as Record<string, unknown>
  const names = ['getBoundingClientRect', 'getClientRects', 'clientWidth'] as const
  const saved = names.map((n) => [n, Object.getOwnPropertyDescriptor(proto, n)] as const)
  Object.defineProperty(proto, 'getBoundingClientRect', { configurable: true, value: () => { onRead?.(); return rect(300) } })
  Object.defineProperty(proto, 'getClientRects', { configurable: true, value: () => [rect(300)] })
  Object.defineProperty(proto, 'clientWidth', { configurable: true, get: () => 100 })
  return () => {
    for (const [n, d] of saved) {
      if (d) Object.defineProperty(proto, n, d)
      else delete proto[n]
    }
  }
}

const cellOf = (identity: string) =>
  [...document.querySelectorAll<HTMLElement>('.quota-hp.claude')].find((el) => el.querySelector('.quota-identity')?.textContent === identity) ?? null

virtualMockTime()
afterEach(async () => {
  await unmountAll()
})
before(setupDom)
after(async () => {
  resetStoreForTest()
  await teardownDom()
})

const it = (name: string, fn: () => Promise<void>) => test(name, { timeout: 30_000 }, fn)

it('收合時每格直向畫出全部窗口（5h 與 7d 兩個節點），不收成單窗口', async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  const restore = squeezeLayout()
  try {
    await mount(<QuotaStrip />)
    await until(() => document.querySelector('.quota-open.collapsed') !== null, '放不下時收合')
    await until(() => cellOf('cc0') !== null, 'cc0 格畫出來')
    const cell = cellOf('cc0')!
    const windows = [...cell.querySelectorAll('.quota-window')]
    assert.ok(windows.length >= 2, `收合後 cc0 仍有 5h 與 7d 兩個窗口，實際 ${windows.length} 個`)
    assert.equal(cell.querySelector('.quota-bars.single'), null, '不是單窗口的版型')
    for (const w of windows) {
      assert.ok(w.querySelector('.quota-bar-wrap'), '每一列都有條')
      assert.ok(w.querySelector('.quota-bar-pct'), '每一列都有數字')
    }
  } finally {
    restore()
  }
})

it('外框寬度不變，資料晚到與字型載入完成仍重新量內容下限', async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  let reads = 0
  const restore = squeezeLayout(() => reads++)
  const originalFonts = Object.getOwnPropertyDescriptor(document, 'fonts')
  const fonts = new EventTarget()
  Object.defineProperty(document, 'fonts', { configurable: true, value: fonts })
  try {
    await mount(<QuotaStrip />)
    const beforeData = reads
    await act(() => useStore.setState({ quota: structuredClone(useStore.getState().quota) }))
    assert.ok(reads > beforeData, '同一組額度格拿到新資料，未收到 resize 也要重量')
    const beforeFont = reads
    await act(() => fonts.dispatchEvent(new Event('loadingdone')))
    assert.ok(reads > beforeFont, '字型載入後自然寬改變，已分配格寬未變仍要重量')
  } finally {
    await unmountAll()
    restore()
    if (originalFonts) Object.defineProperty(document, 'fonts', originalFonts)
    else Reflect.deleteProperty(document, 'fonts')
  }
})

it('收合後仍橫向溢出時標記邊緣漸層，捲動後更新左右漸層', async () => {
  mockApi(sharedMock)
  await useStore.getState().refreshState()
  await useStore.getState().loadQuota()
  const proto = HTMLElement.prototype as unknown as Record<string, unknown>
  const savedSw = Object.getOwnPropertyDescriptor(proto, 'scrollWidth')
  Object.defineProperty(proto, 'scrollWidth', { configurable: true, get: () => 400 })
  const restore = squeezeLayout()
  try {
    await mount(<QuotaStrip />)
    await until(() => document.querySelector('.quota-open.collapsed') !== null, '放不下時收合')
    const row = document.querySelector<HTMLElement>('.quota-open.collapsed')!
    assert.equal(row.getAttribute('data-fade-right'), '1', '一開始右側有漸層')
    assert.equal(row.getAttribute('data-fade-left'), null, '一開始左側無漸層')
    await act(() => {
      row.scrollLeft = 150
      row.dispatchEvent(new Event('scroll'))
    })
    assert.equal(row.getAttribute('data-fade-left'), '1', '捲動中左側有漸層')
    assert.equal(row.getAttribute('data-fade-right'), '1', '未到底右側仍有漸層')
    await act(() => {
      row.scrollLeft = 300
      row.dispatchEvent(new Event('scroll'))
    })
    assert.equal(row.getAttribute('data-fade-left'), '1', '到底左側有漸層')
    assert.equal(row.getAttribute('data-fade-right'), null, '到底右側漸層消失')
  } finally {
    await unmountAll()
    restore()
    if (savedSw) Object.defineProperty(proto, 'scrollWidth', savedSw)
    else delete proto.scrollWidth
  }
})
