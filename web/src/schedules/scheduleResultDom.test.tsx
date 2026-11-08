import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, act } from '../testing/domHarness'
import { ScheduleResult } from './ScheduleResult'
import type { ScheduleOccurrence } from '../api/schedules'
import { occurrenceLine } from './format'

before(setupDom)
after(teardownDom)
afterEach(unmountAll)

test('each attempt shows task outcome and sanitized summary; expansion fetches the exact reply', async () => {
  let opened = 0
  let calls = 0
  fakeApi(({ method, path }) => {
    assert.equal(method, 'GET')
    assert.ok(path.includes('turn_id=t1') && path.includes('role=assistant'))
    calls++
    return { messages: [{ id: 'm1', conversation_id: 'c', turn_id: 't1', role: 'assistant', content: '完整的 synthetic 回覆', source: 'transcript', created_at: '2026-11-08T00:00:00Z' }], has_more: false }
  })
  const o = { id: 'o', bot_id: 'b', status: 'pending', attempt_no: 2, next_retry_at: '2026-11-08T00:03:00Z', attempts: [{ attempt_no: 1, turn_id: 't1', reply_message_id: 'm1', outcome: 'failed', outcome_summary: '失敗 [redacted]' }] } as ScheduleOccurrence
  await mount(<ScheduleResult occurrence={o} onConversation={() => { opened++ }} />)
  assert.ok(document.body.textContent?.includes('任務失敗'))
  assert.ok(document.body.textContent?.includes('第 1 次'))
  assert.ok(document.body.textContent?.includes('等待重試（第 2 次執行）'))
  assert.equal(calls, 0, 'no eager full reply fetch')
  const details = document.querySelector('details')!
  await act(async () => { details.open = true })
  await settle()
  assert.equal(calls, 1)
  assert.ok(document.body.textContent?.includes('完整的 synthetic 回覆'))
  await click(document.querySelector('button')!)
  assert.equal(opened, 1)
  assert.equal(occurrenceLine(o), '等待重試（第 2 次執行）')
})


test('cancelled retry does not display a pending retry or invent a second result', async () => {
  const occurrence = { id: 'cancelled', bot_id: 'b', status: 'cancelled', attempt_no: 2, next_retry_at: '2026-11-08T00:03:00Z', outcome: 'failed', attempts: [{ attempt_no: 1, turn_id: null, reply_message_id: null, outcome: 'failed', outcome_summary: '第一次失敗' }] } as ScheduleOccurrence
  await mount(<ScheduleResult occurrence={occurrence} onConversation={() => {}} />)
  assert.ok(document.body.textContent?.includes('第 1 次'))
  assert.ok(!document.body.textContent?.includes('第 2 次'))
  assert.ok(!document.body.textContent?.includes('等待重試'))
})
