import test from 'node:test'
import assert from 'node:assert/strict'
import { clipAfterRows, hiddenChipIndexes, layoutBoxes, lineBudget, moreTitle, orderChanged, type ChipBox } from './chipOverflow.ts'

const row = (pinned: boolean, tops: number[]): ChipBox[] => tops.map((top) => ({ pinned, top }))

test('主力占滿兩行時，沒釘的要回答仍在第二行，不被收進 +N（review3 c4 L2）', () => {
  // 7 顆主力（一行放 6 顆 → 兩行），第 8 顆是沒釘、停在提示上的 bot（換到下一行）。
  const boxes = [...row(true, [0, 0, 0, 0, 0, 0, 28]), ...row(false, [56])]
  const hidden = hiddenChipIndexes(boxes)
  assert.deepEqual(hidden, [6], '藏的是溢出第一行的主力')
  assert.ok(!hidden.includes(7), '沒釘的那組第一行照樣看得到')
})

test('只有一組時那組最多兩行；兩組都在時各一行', () => {
  assert.equal(lineBudget(true, false), 2)
  assert.equal(lineBudget(false, true), 2)
  assert.equal(lineBudget(true, true), 1)
  assert.deepEqual(hiddenChipIndexes(row(true, [0, 0, 28, 28, 56])), [4])
  assert.deepEqual(hiddenChipIndexes(row(false, [0, 28, 56, 56])), [2, 3])
  assert.deepEqual(hiddenChipIndexes([...row(true, [0, 0]), ...row(false, [28, 28, 56, 84])]), [4, 5])
  assert.deepEqual(hiddenChipIndexes([...row(true, [0]), ...row(false, [28])]), [])
  assert.deepEqual(hiddenChipIndexes([]), [])
})

test('+N 提示照實際藏起來的寫', () => {
  assert.equal(moreTitle([{ needsReply: false, unread: 0 }, { needsReply: false, unread: 0 }]), '還有 2 顆沒顯示（都是比較不急的）。點一下展開')
  assert.equal(
    moreTitle([{ needsReply: true, unread: 0 }, { needsReply: false, unread: 3 }, { needsReply: false, unread: 0 }]),
    '還有 3 顆沒顯示（其中 1 顆要你回答、1 顆有未讀）。點一下展開',
  )
})

test('裁切高度是量出來的：看得到的最後一行的下緣（晶片變高也不會被切一半）', () => {
  // 一行 22px 高、行距 6px：第二行在 28，第三行在 56。
  const boxes = [0, 0, 28, 28, 56].map((top) => ({ top, bottom: top + 22 }))
  assert.deepEqual(clipAfterRows(boxes, 2), { hidden: [4], visibleBottom: 50 })
  assert.deepEqual(clipAfterRows(boxes, 1), { hidden: [2, 3, 4], visibleBottom: 22 })
  assert.deepEqual(clipAfterRows(boxes, 3), { hidden: [], visibleBottom: 0 }, '放得下就不裁')
  // 晶片變高（星號與徽章）：裁切位置跟著變，不是寫死的 46px。
  const taller = [0, 34].map((top) => ({ top, bottom: top + 28 }))
  assert.equal(clipAfterRows(taller, 1).visibleBottom, 28)
})

/** 假晶片：排版位置（offset*）＋ FLIP 動畫中的 transform 位移（只反映在 getBoundingClientRect）。 */
const chip = (offsetTop: number, animDy = 0) => ({
  offsetTop,
  offsetHeight: 22,
  getBoundingClientRect: () => ({ top: offsetTop + animDy, bottom: offsetTop + animDy + 22 }),
})

test('動畫中的晶片照排版位置量：裁切不隨 transform 來回翻（2026-09-23 殘影）', () => {
  // 組的上緣在 40；兩行，一行收合（兩組都在）。第 3 顆正從下一行滑上來（FLIP translate +28）。
  const still = [chip(41), chip(41), chip(69)]
  const moving = [chip(41), chip(41), chip(69, -28)]
  const want = clipAfterRows(layoutBoxes(still, 40), 1)
  assert.deepEqual(want, { hidden: [2], visibleBottom: 23 })
  // 同一個寬度、同一份排版，不管動畫播到哪一幀都得到同一個答案——否則 +N／裁切高度每幀跳，資訊列被拖出殘影。
  for (const dy of [-28, -14, -3, 0, 5]) {
    assert.deepEqual(clipAfterRows(layoutBoxes([chip(41), chip(41), chip(69, dy)], 40), 1), want, `動畫位移 ${dy}px`)
  }
  assert.deepEqual(clipAfterRows(layoutBoxes(moving, 40), 1), want)
})

test('FLIP 只在順序變了時播：換行、+N 出現、晶片增減不算換位', () => {
  assert.equal(orderChanged(['a', 'b', 'c'], ['a', 'b', 'c']), false, '只是換行（寬度變了）')
  assert.equal(orderChanged(['a', 'b', 'c'], ['a', 'b']), false, '少一顆')
  assert.equal(orderChanged(['a', 'c'], ['a', 'b', 'c']), false, '多一顆（插在中間）')
  assert.equal(orderChanged([], ['a', 'b']), false, '第一次畫')
  assert.equal(orderChanged(['a', 'b', 'c'], ['b', 'a', 'c']), true, '拖放換位')
  assert.equal(orderChanged(['a', 'b', 'c'], ['a', 'c', 'x', 'b']), true, '換位同時多一顆')
})
