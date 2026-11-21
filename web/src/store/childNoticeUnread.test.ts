import test from 'node:test'
import assert from 'node:assert/strict'
import { seed, runTurn } from './unreadTurn.harness.ts'
import { useStore } from './store.ts'

for (const crid of ['child-done:child:t', 'child-blocked:child:t']) {
  test(`${crid}：parent 回覆完就算未讀，跟普通回合一樣（使用者 2026-10-16）`, () => {
    seed()
    runTurn('t1', crid, false)
    assert.equal(useStore.getState().botUnread.b1, 1)
    runTurn('t2', 'web-1', false)
    assert.equal(useStore.getState().botUnread.b1, 2)
  })
}
