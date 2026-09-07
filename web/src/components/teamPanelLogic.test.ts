import test from 'node:test'
import assert from 'node:assert/strict'
import type { Team, TeamEvent } from '../api/types.ts'
import { canReopenTeam, describeEvent } from './teamPanelLogic.ts'

const team = (deleted = false) => ({
  id: 'team-1',
  phase: 'done',
  members: [{ bot_id: 'pm-1', role: 'pm', deleted }],
}) as Team

test('done team shows reopen only while the PM is not cleaned up', () => {
  assert.equal(canReopenTeam(team()), true)
  assert.equal(canReopenTeam(team(true)), false)
  assert.equal(canReopenTeam(team(), true), false)
})

test('describeEvent explains reopen and lost native context notes', () => {
  const base = { id: 'e', kind: 'note', from_bot_id: null, to_bot_id: null, task_id: null, turn_id: null, status: null, created_at: '' }
  assert.equal(
    describeEvent({ ...base, payload: { action: 'team_reopened', issue_numbers: [57, 58] } } as TeamEvent),
    '使用者追加 #57、#58，team 重新啟動',
  )
  assert.equal(
    describeEvent({ ...base, payload: { action: 'member_context_lost', bot: 'team-pm', role: 'pm', why: 'no_session_id' } } as TeamEvent),
    'PM 沒能續接先前對話，已改為新對話',
  )
})

test('describeEvent translates the reopen phase reason instead of printing the code', () => {
  const ev = {
    id: 'e2',
    kind: 'phase',
    payload: { from: 'done', to: 'starting', reason: 'reopen' },
    from_bot_id: null,
    to_bot_id: null,
    task_id: null,
    turn_id: null,
    status: null,
    created_at: '',
  } as TeamEvent
  assert.equal(describeEvent(ev), '已完成 → 啟動中（使用者追加 issue）')
})
