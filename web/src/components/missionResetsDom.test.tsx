/**
 * 驗證者找不到 Fable 額度時的重置清單：重置時間讀不到的身分不能印成「額度  回來」（中間空白、意思還相反）（#1133）。
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
  on_5h_limit: 'wait', max_rounds: 2, rounds_used: 0, paused_reason: 'no_fable_for_verifier', paused_detail: null, result_summary: null,
  parent_mission_id: null, status: 'paused', phase: 'paused', created_at: '2026-10-01T00:00:00.000Z',
  updated_at: '2026-10-01T00:00:00.000Z', completed_at: null, cancelled_at: null,
}
const detail = {
  ...mission, assignments: [], revisions: [], parent: null,
  events: [{
    id: 'e1', mission_id: 'm1', kind: 'paused', text: '', relay_from: 'daemon', reply_to: null, created_at: '2026-10-01T00:05:00.000Z',
    payload: {
      reason: 'no_fable_for_verifier',
      decision: {
        decision: 'ask_user', reason: 'x',
        resets: [{ identity: 'cc1', resets_at: null }, { identity: 'cc2', resets_at: '2026-10-10T06:00:00Z' }],
      },
    },
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

test('重置時間讀不到的身分不印成空白，有時間的照舊', async () => {
  await mount(<MissionsBar projectId="p1" />)
  await settle(200)
  const rows = [...document.querySelectorAll<HTMLElement>('.mission-resets li')].map((li) => li.textContent!)
  assert.equal(rows.length, 2, `${rows}`)
  assert.equal(rows[0], 'cc1 Fable 額度用完了，重置時間不明')
  assert.match(rows[1], /^cc2 Fable 額度 \d+\/\d+ \d{2}:\d{2} 回來$/)
  for (const r of rows) assert.ok(!/ {2}/.test(r), `不能有連續空白：${JSON.stringify(r)}`)
})
