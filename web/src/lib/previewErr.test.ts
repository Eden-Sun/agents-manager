import test from 'node:test'
import assert from 'node:assert/strict'
import { shownPreviewErr } from './previewErr.ts'

/**
 * 預覽面板的錯誤有兩個來源：使用者按啟動／停止失敗（`start`），與背景讀預覽狀態失敗（`load`）。
 * 以前共用一格，讀取失敗後即使下一次讀成功，那條紅色警示還釘在已經跑起來的預覽上面。
 */
test('按鈕失敗的錯誤優先；讀取失敗的錯誤讀成功之後就沒有了', () => {
  assert.equal(shownPreviewErr('start failed', 'load failed'), 'start failed')
  assert.equal(shownPreviewErr(null, 'load failed'), 'load failed')
  assert.equal(shownPreviewErr(null, null), null, '讀成功（load 清掉）而且沒有按鈕錯誤：不顯示')
})
