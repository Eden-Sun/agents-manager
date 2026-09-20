import assert from 'node:assert/strict'
import { test } from 'node:test'
import { dropBefore, moveBefore, moveStep, sortPinned, type Box } from './pinnedOrder'

test('sortPinned: 照 primary_position，同值照原順序', () => {
  const items = [
    { id: 'a', position: 0, index: 0 },
    { id: 'b', position: 0, index: 1 },
    { id: 'c', position: 2, index: 2 },
    { id: 'd', position: 1, index: 3 },
  ]
  assert.deepEqual(sortPinned(items).map((x) => x.id), ['a', 'b', 'd', 'c'])
})

test('moveBefore: 前移、移到最後、沒變動與未知 id 回 null', () => {
  assert.deepEqual(moveBefore(['a', 'b', 'c'], 'c', 'a'), ['c', 'a', 'b'])
  assert.deepEqual(moveBefore(['a', 'b', 'c'], 'a', null), ['b', 'c', 'a'])
  assert.equal(moveBefore(['a', 'b', 'c'], 'a', 'b'), null)
  assert.equal(moveBefore(['a', 'b'], 'x', 'a'), null)
  assert.equal(moveBefore(['a', 'b'], 'a', 'zzz'), null)
})

test('moveStep: 鍵盤左右一格，頭尾不動', () => {
  assert.deepEqual(moveStep(['a', 'b', 'c'], 'b', -1), ['b', 'a', 'c'])
  assert.deepEqual(moveStep(['a', 'b', 'c'], 'b', 1), ['a', 'c', 'b'])
  assert.equal(moveStep(['a', 'b', 'c'], 'a', -1), null)
  assert.equal(moveStep(['a', 'b', 'c'], 'c', 1), null)
})

const box = (id: string, left: number, top: number): Box => ({ id, left, top, right: left + 80, bottom: top + 24 })

test('dropBefore: 單行看中心點、行尾接最後', () => {
  const boxes = [box('a', 0, 0), box('b', 90, 0), box('c', 180, 0)]
  assert.equal(dropBefore(boxes, 10, 10, 'c'), 'a')
  assert.equal(dropBefore(boxes, 100, 10, 'c'), 'b')
  assert.equal(dropBefore(boxes, 250, 10, 'a'), null)
  // 拖的那顆自己不算：a 拖到 b 的左半邊＝原位，由 moveBefore 擋成 null
  assert.equal(dropBefore(boxes, 95, 10, 'a'), 'b')
})

test('dropBefore: 換行時依游標所在的行；行尾接到下一行第一顆之前', () => {
  const boxes = [box('a', 0, 0), box('b', 90, 0), box('c', 0, 30), box('d', 90, 30)]
  assert.equal(dropBefore(boxes, 10, 35, 'a'), 'c')
  assert.equal(dropBefore(boxes, 200, 5, 'd'), 'c')
  assert.equal(dropBefore(boxes, 200, 35, 'a'), null)
  assert.equal(dropBefore(boxes, 300, 200, 'a'), null)
})
