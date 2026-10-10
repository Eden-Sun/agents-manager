/** 新增 Project 的標籤自動帶入（#1084）：換目錄要跟著換，使用者自己打的不動。 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { dirLabel, labelAfterPick } from './projectLabel'

test('dirLabel：路徑最後一段；根目錄與空字串是空的', () => {
  assert.equal(dirLabel('/a/b'), 'b')
  assert.equal(dirLabel('/a/b/'), 'b')
  assert.equal(dirLabel('/'), '')
  assert.equal(dirLabel(''), '')
})

test('標籤是空的：帶入目錄名', () => {
  assert.equal(labelAfterPick('', '', '/x/foo'), 'foo')
  assert.equal(labelAfterPick('  ', '', '/x/foo'), 'foo')
})

test('還是上一次自動帶入的：換成新目錄名', () => {
  assert.equal(labelAfterPick('foo', 'foo', '/x/bar'), 'bar')
})

test('使用者自己打的不動', () => {
  assert.equal(labelAfterPick('我的專案', 'foo', '/x/bar'), '我的專案')
  assert.equal(labelAfterPick('foo2', 'foo', '/x/bar'), 'foo2')
})
