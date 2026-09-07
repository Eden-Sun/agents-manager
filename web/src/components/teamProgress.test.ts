import test from 'node:test'
import assert from 'node:assert/strict'
import type { Team, TeamIssue } from '../api/types.ts'
import { fmtDur, issueQueueAt, teamProgressOf } from './teamProgress.ts'

const issue = (over: Partial<TeamIssue>): TeamIssue =>
  ({ id: 'i1', seq: 1, state: 'working', started_at: null, ended_at: null, ...over }) as TeamIssue

const team = (over: Partial<Team>): Team =>
  ({
    issues: [],
    current_issue_id: null,
    issues_summary: { total: 0, done: 0, failed: 0, queued: 0 },
    tasks_summary: { total: 0 },
    started_at: null,
    ended_at: null,
    ...over,
  }) as Team

test('佇列走到第幾個：交付 1 個、正在做第 2 個 → 2', () => {
  const t = team({
    issues: [issue({ id: 'a', state: 'done' }), issue({ id: 'b', state: 'working' })],
    current_issue_id: 'b',
    issues_summary: { total: 20, done: 1, failed: 0, queued: 18 },
  })
  assert.equal(issueQueueAt(t), 2)
  const p = teamProgressOf(t)
  assert.equal(p.kind, 'issues')
  assert.equal(p.at, 2)
  assert.equal(p.total, 20)
})

test('佇列全部結束時分子等於分母，不會超過', () => {
  const t = team({
    issues: [issue({ id: 'a', state: 'done' }), issue({ id: 'b', state: 'failed' })],
    current_issue_id: 'b',
    issues_summary: { total: 2, done: 1, failed: 1, queued: 0 },
  })
  assert.equal(issueQueueAt(t), 2)
})

test('單一 issue 改數 task：已結案的是 merged / skipped / failed', () => {
  const p = teamProgressOf(
    team({
      issues_summary: { total: 1, done: 0, failed: 0, queued: 0 },
      tasks_summary: { total: 5, merged: 2, skipped: 1, working: 1, queued: 1 },
    }),
  )
  assert.equal(p.kind, 'tasks')
  assert.equal(p.at, 3)
  assert.equal(p.total, 5)
})

test('PM 還沒拆 task 時 total 是 0，呼叫端就不畫計數', () => {
  const p = teamProgressOf(team({ issues_summary: { total: 1, done: 0, failed: 0, queued: 0 } }))
  assert.equal(p.total, 0)
})

test('計時從當前 issue 的 started_at 起算，佇列裡沒有就退回整隊', () => {
  const withIssue = teamProgressOf(
    team({
      issues: [issue({ id: 'b', started_at: '2026-09-08T10:00:00Z' })],
      current_issue_id: 'b',
      started_at: '2026-09-08T09:00:00Z',
    }),
  )
  assert.equal(withIssue.startedAt, Date.parse('2026-09-08T10:00:00Z'))
  const noIssue = teamProgressOf(team({ started_at: '2026-09-08T09:00:00Z' }))
  assert.equal(noIssue.startedAt, Date.parse('2026-09-08T09:00:00Z'))
})

test('整隊結束就凍結，即使那一項自己沒寫 ended_at', () => {
  const p = teamProgressOf(
    team({
      issues: [issue({ id: 'b', started_at: '2026-09-08T10:00:00Z' })],
      current_issue_id: 'b',
      started_at: '2026-09-08T09:00:00Z',
      ended_at: '2026-09-08T11:00:00Z',
    }),
  )
  assert.equal(p.endedAt, Date.parse('2026-09-08T11:00:00Z'))
})

test('還沒開跑的 team 沒有起算點', () => {
  assert.equal(teamProgressOf(team({})).startedAt, null)
})

test('fmtDur：秒 / 分秒 / 小時分', () => {
  assert.equal(fmtDur(12_000), '12 秒')
  assert.equal(fmtDur(192_000), '3 分 12 秒')
  assert.equal(fmtDur(7_800_000), '2 小時 10 分')
  assert.equal(fmtDur(-5), '0 秒')
})
