import test from 'node:test'
import assert from 'node:assert/strict'
import type { Team, TeamEvent, TeamPauseDetail, TeamPauseQuotaMember } from '../api/types.ts'
import {
  canReopenTeam,
  describeEvent,
  teamPauseAction,
  teamPauseDetailLines,
  teamPauseText,
  teamQuotaMemberText,
} from './teamPanelLogic.ts'

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

test('側欄暫停列：預算類加碼再繼續，要人回話／放行／救成員的不給按鈕', () => {
  assert.equal(teamPauseAction('budget_time'), 'bump')
  assert.equal(teamPauseAction('budget_relays'), 'bump')
  assert.equal(teamPauseAction('user'), 'resume')
  assert.equal(teamPauseAction('quota_low'), 'resume')
  assert.equal(teamPauseAction(null), 'resume')
  assert.equal(teamPauseAction('ask_user'), null)
  assert.equal(teamPauseAction('gate:merge'), null)
  assert.equal(teamPauseAction('member_lost:dev-1'), null)
})

const quotaMember = (over: Partial<TeamPauseQuotaMember> = {}): TeamPauseQuotaMember => ({
  bot_id: 'b-rev',
  name: 'ttxka1d-i2-rev',
  short: 'rev',
  role: 'reviewer',
  kind: 'claude',
  identity: 'cc2',
  host: 'local',
  window: 'five_hour',
  used_pct: 96,
  remaining_pct: 4,
  resets_at: null,
  ...over,
})

const pausedTeam = (detail: TeamPauseDetail | null, reason: string | null = 'quota_low') =>
  ({ id: 'team-1', phase: 'paused', pause_reason: reason, pause_detail: detail, members: [] }) as unknown as Team

test('額度暫停要寫出是誰、哪個視窗、剩多少', () => {
  assert.equal(teamQuotaMemberText(quotaMember()), 'rev（cc2）5h 額度剩 4%')
  // 沒有 identity 的（那個 kind 只有一個帳號）退回寫 kind，不能只剩一個角色名。
  assert.equal(
    teamQuotaMemberText(quotaMember({ short: 'dev-1', identity: null, window: 'seven_day', remaining_pct: 2.4 })),
    'dev-1（claude）7d 額度剩 2%',
  )
  // grok 的長週期在 UI 一律叫「週」，跟額度列同一套說法。
  assert.equal(
    teamQuotaMemberText(quotaMember({ short: 'pm', kind: 'grok', identity: null, window: 'seven_day' })),
    'pm（grok）週 額度剩 4%',
  )
  assert.match(teamQuotaMemberText(quotaMember({ resets_at: '2026-09-09T03:20:00Z' })), /才 reset$/)
})

test('橫幅：一位就寫那一位，多位寫最嚴重的 + 另有 N 位；沒細節退回舊文案', () => {
  const one = { stop_pct: 90, members: [quotaMember()] }
  assert.equal(teamPauseText(pausedTeam(one)), 'rev（cc2）5h 額度剩 4%')
  const two = { stop_pct: 90, members: [quotaMember(), quotaMember({ short: 'dev-1', bot_id: 'b-dev1' })] }
  assert.equal(teamPauseText(pausedTeam(two)), 'rev（cc2）5h 額度剩 4%（另有 1 位額度也不足）')
  assert.equal(teamPauseDetailLines(pausedTeam(two)).split('\n').length, 2)
  // 舊 daemon（沒有 pause_detail）與其他原因都還是機器碼的中文。
  assert.equal(teamPauseText(pausedTeam(null)), '額度過低')
  assert.equal(teamPauseText(pausedTeam(one, 'budget_time')), '時間預算用完')
})
