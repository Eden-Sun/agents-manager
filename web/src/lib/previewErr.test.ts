import test from 'node:test'
import assert from 'node:assert/strict'
import { shownPreviewErr } from './previewErr.ts'

test('按鈕失敗的錯誤優先；讀取失敗的錯誤讀成功之後就沒有了', () => {
  assert.equal(shownPreviewErr('start failed', 'load failed'), 'start failed')
  assert.equal(shownPreviewErr(null, 'load failed'), 'load failed')
  assert.equal(shownPreviewErr(null, null), null, '讀成功（load 清掉）而且沒有按鈕錯誤：不顯示')
})
