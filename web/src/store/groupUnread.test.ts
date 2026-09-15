import test from 'node:test'
import assert from 'node:assert/strict'
import { isGroupTurn, noteGroupPrompt, noteGroupPrompts, resetGroupTurnsForTest } from './groupUnread.ts'

test('只有回群組訊息的那一回合算群組回覆；單獨對話、沒有 turn 的不算', () => {
  resetGroupTurnsForTest()
  noteGroupPrompt({ role: 'user', group_id: 'g1', turn_id: 't-group' })
  noteGroupPrompt({ role: 'user', group_id: null, turn_id: 't-direct' })
  noteGroupPrompt({ role: 'assistant', group_id: 'g1', turn_id: 't-assistant' })
  noteGroupPrompt({ role: 'user', group_id: 'g2', turn_id: null })
  assert.equal(isGroupTurn('t-group'), true)
  assert.equal(isGroupTurn('t-direct'), false)
  assert.equal(isGroupTurn('t-assistant'), false)
  assert.equal(isGroupTurn(null), false)
})

test('載入的群組時間軸也會登記；超過上限丟最舊的', () => {
  resetGroupTurnsForTest()
  noteGroupPrompts(Array.from({ length: 501 }, (_, i) => ({ role: 'user' as const, group_id: 'g', turn_id: `t${String(i).padStart(3, '0')}` })))
  assert.equal(isGroupTurn('t000'), false)
  assert.equal(isGroupTurn('t500'), true)
})
