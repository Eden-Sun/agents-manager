/**
 * 任務卡「回答 AGM」的輸入框有無障礙名稱，並指向 AGM 的問句（#1132）。只有 placeholder 讀屏只會唸「文字區域」。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { MissionsBar } from './MissionsBar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const mission = {
  id: 'm1', project_id: 'p1', client_request_id: 'r1', text: '整理文件', delivery_mode: 'push_main', executor_kind: 'claude',
  on_5h_limit: 'wait', max_rounds: 2, rounds_used: 2, paused_reason: 'max_rounds', paused_detail: null, result_summary: null,
  parent_mission_id: null, status: 'paused', phase: 'paused', created_at: '2026-10-01T00:00:00.000Z',
  updated_at: '2026-10-01T00:00:00.000Z', completed_at: null, cancelled_at: null,
}
const detail = {
  ...mission, assignments: [], revisions: [], parent: null,
  events: [{
    id: 'e1', mission_id: 'm1', kind: 'paused', text: '要繼續嗎？', relay_from: 'daemon', payload: {}, reply_to: null,
    created_at: '2026-10-01T00:05:00.000Z',
  }],
}

const route = (req: FakeRequest) => {
  if (req.path.includes('/missions/m1')) return detail
  if (req.path.includes('/projects/p1/missions')) return [mission]
  return undefined
}

beforeEach(() => {
  fakeApi(route)
  useStore.setState({ missionsSupported: true, missions: { p1: [mission] }, missionDetail: { m1: detail } } as never)
})

test('停下來問人的回答框有可及名稱，並指向 AGM 的問句', async () => {
  await mount(<MissionsBar projectId="p1" />)
  await settle(200)
  const ta = document.querySelector<HTMLTextAreaElement>('textarea.mission-answer')!
  assert.ok(ta, '要有回答框')
  assert.equal(ta.getAttribute('aria-label'), '回答 AGM')
  const id = ta.getAttribute('aria-describedby')
  assert.ok(id, '要指向問句')
  assert.equal(document.getElementById(id!)?.textContent, '要繼續嗎？')
})
