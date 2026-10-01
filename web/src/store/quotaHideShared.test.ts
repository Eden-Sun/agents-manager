/**
 * 額度停用存在所有分頁共用的 localStorage：整份覆寫會洗掉別的分頁剛勾的（#755）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { mergeDisabled } from './quotaHide.ts'

test('分頁 B 勾 cc2 不會洗掉分頁 A 剛勾的 cc1', () => {
  const disk = { 'local|claude|cc1': null } // A 已經寫進磁碟
  const prevB = {} // B 的記憶體還沒有它
  const nextB = { 'local|claude|cc2': 1_700_000_000_000 }
  assert.deepEqual(mergeDisabled(prevB, nextB, disk), { 'local|claude|cc1': null, 'local|claude|cc2': 1_700_000_000_000 })
})

test('解除只刪這個分頁動過的鍵，別的分頁新增的留著', () => {
  const disk = { 'local|claude|cc1': null, 'local|claude|cc3': null }
  const prev = { 'local|claude|cc1': null }
  assert.deepEqual(mergeDisabled(prev, {}, disk), { 'local|claude|cc3': null })
})

test('改了到期時間的鍵才覆寫，沒動的鍵跟著磁碟走', () => {
  const disk = { a: 5, b: null }
  assert.deepEqual(mergeDisabled({ a: 1, b: null }, { a: 1, b: null, c: null }, disk), { a: 5, b: null, c: null })
  assert.deepEqual(mergeDisabled({ a: 1 }, { a: 9 }, disk), { a: 9, b: null })
})
