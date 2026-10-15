import test, { after, before } from 'node:test'
import assert from 'node:assert/strict'
import { setupDom, teardownDom } from '../testing/domHarness'
import { measureQuotaAvail, measureQuotaRow, quotaRowSize } from './quotaMeasure'
import { QUOTA_FIT_START, nextQuotaFit } from './quotaLayout'

before(setupDom)
after(teardownDom)

test('等寬需要以最寬格乘格數，不能加總各格內容寬而提早展開', () => {
  const size = quotaRowSize([96, 101, 109, 102, 101], 0, 0, 5)
  assert.deepEqual(size, { cellMin: 109, width: 545 })
  assert.equal(nextQuotaFit(QUOTA_FIT_START, 530, size.width, false).level, 2)
  assert.equal(nextQuotaFit(QUOTA_FIT_START, 545, size.width, false).level, 0)
})

test('停用格、更新 chip、內距與間距獨立保留，不均分為啟用格寬', () => {
  assert.deepEqual(quotaRowSize([100, 120], 50, 4, 4), { cellMin: 120, width: 302 })
  assert.deepEqual(quotaRowSize([], 0, 4, 0), { cellMin: 0, width: 0 })
})

test('DOM 量測不受已分配的 flex 寬影響，輸出每格內容下限並完整還原樣式（#1780）', () => {
  const row = document.createElement('div')
  row.style.cssText = 'padding:0 3px;column-gap:4px'
  document.body.append(row)
  const widths = [96, 101, 109, 102, 101, 20, 30]
  const saved: (string | null)[] = []
  for (const [i, width] of widths.entries()) {
    const el = document.createElement('span')
    el.className = i < 5 ? 'quota-hp' : i === 5 ? 'quota-hp off' : 'quota-update'
    if (i === 0) el.style.cssText = 'flex:1 0 0;min-width:200px;color:red'
    if (i === 6) el.style.marginRight = '5px'
    saved.push(el.getAttribute('style'))
    el.getClientRects = () => [new DOMRect(0, 0, width, 20)] as unknown as DOMRectList
    el.getBoundingClientRect = () => {
      assert.equal(el.style.flex, '0 0 auto', 'basis:0 與 shrink 必須一併解除，才量得到內容')
      assert.equal(el.style.minWidth, 'max-content', '上一次的下限不可阻止內容縮小後重量')
      return new DOMRect(0, 0, width, 20)
    }
    row.append(el)
  }
  try {
    assert.deepEqual(measureQuotaRow(row), { cellMin: 109, width: 630 })
    assert.deepEqual([...row.children].map(el => el.getAttribute('style')), saved)
  } finally {
    row.remove()
  }
})

test('可用寬扣掉實際記憶體徽章與分頁，隱藏 spacer 不佔寬也不吃 gap', () => {
  const head = document.createElement('div')
  head.style.cssText = 'padding:0 16px;column-gap:10px'
  Object.defineProperty(head, 'clientWidth', { value: 1000 })
  document.body.append(head)
  const strip = document.createElement('div')
  for (const [name, width] of [['main-title', 252], ['quota-strip', 0], ['mem-wrap', 90], ['spacer', 0], ['tabs', 110]] as const) {
    const el = name === 'quota-strip' ? strip : document.createElement('div')
    el.className = name
    el.getClientRects = () => (name === 'spacer' ? [] : [new DOMRect()]) as unknown as DOMRectList
    el.getBoundingClientRect = () => new DOMRect(0, 0, width, 60)
    head.append(el)
  }
  try {
    assert.equal(measureQuotaAvail(head, strip), 486)
  } finally {
    head.remove()
  }
})
