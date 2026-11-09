import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ProjectSchedules } from './ProjectSchedules'
import { ScheduleResult } from './ScheduleResult'
import { formatDuration, totalDurationMs } from './duration'
import type { ScheduleOccurrence } from '../api/schedules'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

test('formatDuration: 秒、分秒、時分', () => {
  assert.equal(formatDuration(14_000), '14 秒')
  assert.equal(formatDuration(134_000), '2 分 14 秒')
  assert.equal(formatDuration(3_900_000), '1 小時 5 分')
  assert.equal(formatDuration(-5), '0 秒')
})

test('totalDurationMs: 還沒開始是 null；進行中加上已進行時間', () => {
  assert.equal(totalDurationMs({}, 0), null)
  assert.equal(totalDurationMs({ duration_ms: 60_000 }, 0), 60_000)
  const since = '2026-11-09T01:00:00.000Z'
  assert.equal(totalDurationMs({ duration_ms: 60_000, running_since: since }, Date.parse(since) + 14_000), 74_000)
})

const occ = (patch: Partial<ScheduleOccurrence>) => ({ id: 'o', bot_id: 'b', status: 'done', attempt_no: 2, ...patch }) as ScheduleOccurrence

test('結果詳情：總花費時間與各 attempt 耗時', async () => {
  const o = occ({
    duration_ms: 134_000, outcome: 'success',
    attempts: [
      { attempt_no: 1, turn_id: 't1', reply_message_id: null, outcome: 'failed', outcome_summary: '失敗', duration_ms: 60_000 },
      { attempt_no: 2, turn_id: 't2', reply_message_id: null, outcome: 'success', outcome_summary: '完成', duration_ms: 74_000 },
    ],
  })
  await mount(<ScheduleResult occurrence={o} onConversation={() => {}} />)
  const text = document.body.textContent ?? ''
  assert.match(text, /總花費時間：2 分 14 秒/)
  assert.match(text, /第 1 次 · 耗時 1 分 0 秒/)
  assert.match(text, /第 2 次 · 耗時 1 分 14 秒/)
})

test('結果詳情：進行中顯示已進行時間', async () => {
  const since = new Date(Date.now() - 30_000).toISOString()
  await mount(<ScheduleResult occurrence={occ({ status: 'queued', duration_ms: 0, running_since: since, attempts: [] })} onConversation={() => {}} />)
  assert.match(document.body.textContent ?? '', /已進行 (29|30|31) 秒/)
})

test('結果詳情：還沒開始不顯示花費時間', async () => {
  await mount(<ScheduleResult occurrence={occ({ status: 'pending', attempts: [] })} onConversation={() => {}} />)
  assert.ok(!document.body.querySelector('.schedule-total-duration'))
})

test('排程列表：「上次」旁顯示上次執行的總花費時間', async () => {
  const base = { project_id: 'p1', bot_id: 'b1', prompt: 'x', tz: 'UTC', mode: 'daily', spec: { mode: 'daily', time: '09:00' }, next_due: '2026-11-10T09:00:00.000Z', enabled: true, generation: 1, notify: 'failure', created_at: '', updated_at: '', open: null }
  fakeApi(() => ({ schedules: [
    { ...base, id: 's1', name: '有紀錄', last: occ({ duration_ms: 134_000 }) },
    { ...base, id: 's2', name: '沒紀錄', last: null },
  ] }))
  await mount(<ProjectSchedules projectId="p1" onOpen={() => {}} />)
  await settle()
  const rows = document.querySelectorAll('.project-schedule-item')
  assert.match(rows[0].textContent ?? '', /上次 2 分 14 秒/)
  assert.ok(!(rows[1].textContent ?? '').includes('上次'))
})
