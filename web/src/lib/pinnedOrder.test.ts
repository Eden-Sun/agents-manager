import assert from 'node:assert/strict'
import { test } from 'node:test'
import { dropBefore, moveBefore, moveStep, pickSlot, sortPinned, DROP_STICKY_PX, type Box } from './pinnedOrder'

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

test('pickSlot: 選最近的插入縫隙，命中區到相鄰晶片的中線；縫隙以右的同行晶片要讓位', () => {
  const boxes = [box('a', 0, 0), box('b', 90, 0), box('c', 180, 0)]
  // a 與 b 的縫隙約在 85；cursor 在 b 的左 1/3（x=105）仍算「b 前面」，在 a 的右半（x=60）才算「a 之後」。
  assert.deepEqual(pickSlot(boxes, 80, 10, 'c'), { before: 'b', shift: ['b'], after: null })
  assert.deepEqual(pickSlot(boxes, 140, 10, 'a'), { before: 'c', shift: ['c'], after: null })
  assert.deepEqual(pickSlot(boxes, 400, 10, 'a'), { before: null, shift: [], after: 'c' }, '游標遠在行尾外側＝最後，標示在最後一顆右邊')
  assert.deepEqual(pickSlot(boxes, -50, 10, 'c'), { before: 'a', shift: ['a', 'b'], after: null }, '行首外側＝最前面')
  // 上下離很遠也鎖定最近那一行（寬鬆的垂直範圍）
  assert.equal(pickSlot(boxes, 100, 400, 'c').before, 'b')
})

test('pickSlot: 遲滯——離目前落點不比離新落點遠超過 DROP_STICKY_PX 就不換', () => {
  const boxes = [box('a', 0, 0), box('b', 90, 0), box('c', 180, 0)]
  // 縫隙 a|b 在 85、b|c 在 175；x=131 離 175 較近（44 < 46），但目前鎖在 b（縫隙 85，距離 46）——差 2px 內不換。
  assert.equal(pickSlot(boxes, 131, 10, 'x', 'b').before, 'b')
  assert.equal(pickSlot(boxes, 131, 10, 'x').before, 'c', '沒有前一個落點就照最近的')
  // 超過遲滯就換。
  assert.equal(pickSlot(boxes, 131 + DROP_STICKY_PX + 10, 10, 'x', 'b').before, 'c')
})

test('pickSlot: 換行——行尾落點接到下一行第一顆之前，讓位只在同一行', () => {
  const boxes = [box('a', 0, 0), box('b', 90, 0), box('c', 0, 30), box('d', 90, 30)]
  assert.deepEqual(pickSlot(boxes, 200, 5, 'd'), { before: 'c', shift: [], after: 'b' }, '第一行行尾：標示在 b 右邊，不是 c 左邊')
  assert.deepEqual(pickSlot(boxes, 95, 35, 'a'), { before: 'd', shift: ['d'], after: null })
})
