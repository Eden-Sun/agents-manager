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

test('長期切換 bot 時，最早的元件 closure 會從快取淘汰', () => {
  const first = markdownComponents('many-bots-0').img
  for (let i = 1; i <= 400; i++) markdownComponents(`many-bots-${i}`)
  assert.notEqual(markdownComponents('many-bots-0').img, first, '無上限的 bot id Map 會一直留住舊 bot 與它的元件 closure')
})
