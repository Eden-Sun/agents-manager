import test from 'node:test'
import assert from 'node:assert/strict'
import { pastedFiles } from './pasteFiles'

const png = () => new File([new Uint8Array([1])], 'image.png', { type: 'image/png' })
const pdf = () => new File([new Uint8Array([1])], 'report.pdf', { type: 'application/pdf' })
function data(files: File[], types: string[], values: Record<string, string> = {}) {
  return { files, types, getData: (type: string) => values[type] ?? '' }
}

test('Excel：帶 RTF、HTML、純文字與 png 截圖時當文字貼上', () => {
  assert.deepEqual(pastedFiles(data([png()], ['text/plain', 'text/html', 'text/rtf', 'Files'], { 'text/plain': 'A\tB' })), [])
})

test('螢幕截圖只有 Files 時仍收附件', () => {
  const file = png()
  assert.deepEqual(pastedFiles(data([file], ['Files'])), [file])
})

test('瀏覽器複製圖片沒有 RTF 時仍收附件', () => {
  const file = png()
  assert.deepEqual(pastedFiles(data([file], ['text/html', 'Files'])), [file])
})

test('檔案總管複製 pdf 時收附件', () => {
  const file = pdf()
  assert.deepEqual(pastedFiles(data([file], ['text/plain', 'Files'], { 'text/plain': 'report.pdf' })), [file])
})

test('RTF 但純文字空白時仍收圖片附件', () => {
  const file = png()
  assert.deepEqual(pastedFiles(data([file], ['text/plain', 'text/rtf', 'Files'])), [file])
})

test('RTF 有字但檔案不是圖片時仍收附件', () => {
  const file = pdf()
  assert.deepEqual(pastedFiles(data([file], ['text/plain', 'text/rtf', 'Files'], { 'text/plain': 'report' })), [file])
})

test('null 或沒有 files 時回空陣列', () => {
  assert.deepEqual(pastedFiles(null), [])
  assert.deepEqual(pastedFiles(data([], ['text/plain'], { 'text/plain': 'hello' })), [])
})
