import test from 'node:test'
import assert from 'node:assert/strict'
import { popDecision } from './popGuard'

function run(over: Partial<Parameters<typeof popDecision>[0]> & { blocks?: boolean }) {
  let asked = 0
  const decision = popDecision({
    drawerOpen: false,
    dialogOpen: false,
    leavingSettings: false,
    guardBlocks: () => {
      asked++
      return over.blocks ?? false
    },
    ...over,
  })
  return { decision, asked }
}

test('對話框開著：上一頁關對話框（抽屜開不開都一樣），守門不問', () => {
  assert.deepEqual(run({ dialogOpen: true }), { decision: 'close-dialog', asked: 0 })
  assert.deepEqual(run({ dialogOpen: true, drawerOpen: true, leavingSettings: true, blocks: true }), { decision: 'close-dialog', asked: 0 })
})

test('只有抽屜開著：關抽屜，守門不問', () => {
  assert.deepEqual(run({ drawerOpen: true }), { decision: 'close-drawer', asked: 0 })
  assert.deepEqual(run({ drawerOpen: true, leavingSettings: true, blocks: true }), { decision: 'close-drawer', asked: 0 })
})

test('離開設定且守門擋下（有未儲存變更）：留在原地，守門只問一次', () => {
  assert.deepEqual(run({ leavingSettings: true, blocks: true }), { decision: 'stay', asked: 1 })
})

test('離開設定但沒有未儲存變更：照常導覽，守門問一次', () => {
  assert.deepEqual(run({ leavingSettings: true, blocks: false }), { decision: 'navigate', asked: 1 })
})

test('沒有要離開設定：導覽，守門一次都不該被叫（它會順手打開確認框）', () => {
  assert.deepEqual(run({ blocks: true }), { decision: 'navigate', asked: 0 })
  assert.deepEqual(run({}), { decision: 'navigate', asked: 0 })
})
