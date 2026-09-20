import test from 'node:test'
import assert from 'node:assert/strict'
import { markdownComponents } from './markdownComponents.tsx'

test('同一顆 bot 每次拿到同一個 img 元件：換一個元件型別，React 會把訊息裡的圖整個卸掉重掛、重抓', () => {
  assert.equal(markdownComponents('b1').img, markdownComponents('b1').img)
  assert.equal(markdownComponents(null).img, markdownComponents(undefined).img)
})

test('不同 bot 各自一個（圖要從各自的專案目錄讀）', () => {
  assert.notEqual(markdownComponents('b1').img, markdownComponents('b2').img)
})
