import test from 'node:test'
import assert from 'node:assert/strict'
import { openCountFromPage } from './issueCount.ts'

test('一頁抓滿（50）：保留 limit=100 那次算出的計數，不蓋成 50', () => {
  assert.equal(openCountFromPage(73, 50), 73)
  assert.equal(openCountFromPage(100, 50), 100)
})

test('沒抓滿：這就是完整筆數，照新的', () => {
  assert.equal(openCountFromPage(73, 12), 12)
  assert.equal(openCountFromPage(null, 0), 0)
})
