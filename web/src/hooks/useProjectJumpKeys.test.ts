import test from 'node:test'
import assert from 'node:assert/strict'
import { jumpTargetId } from './useProjectJumpKeys.ts'

const all = [{ id: 'p1' }, { id: 'p2' }, { id: 'p3' }, { id: 'p4' }, { id: 'p5' }]

test('搜尋時 Control+n 跳到側欄畫出來的第 n 個，不是被藏起來的那個（review3 c5 L4）', () => {
  // 搜「codex」之後側欄只剩原本第 2、第 5 個專案。
  const shown = ['p2', 'p5']
  assert.equal(jumpTargetId(0, shown, all), 'p2')
  assert.equal(jumpTargetId(1, shown, all), 'p5')
  assert.equal(jumpTargetId(2, shown, all), null, '畫面上沒有第 3 個就不動')
})

test('側欄不在畫面上才退回全部專案的順序', () => {
  assert.equal(jumpTargetId(0, null, all), 'p1')
  assert.equal(jumpTargetId(8, null, all), null)
  assert.equal(jumpTargetId(0, [], all), null, '搜尋沒有命中：什麼都不選')
})
