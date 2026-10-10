/**
 * 任務卡的角色列與交辦列顯示 bot 名字而不是 26 碼 id（#1131）。詳情只給 `target_bot_id`，名字要從 store 的 bot 清單對。
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

const BOT_ID = '01HZZZZZZZZZZZZZZZZZZZZZZZ'
const mission = {
  id: 'm1', project_id: 'p1', client_request_id: 'r1', text: '整理文件', delivery_mode: 'push_main', executor_kind: 'claude',
  on_5h_limit: 'wait', max_rounds: 2, rounds_used: 0, paused_reason: null, paused_detail: null, result_summary: null,
  parent_mission_id: null, status: 'open', phase: 'executing', created_at: '2026-10-01T00:00:00.000Z',
  updated_at: '2026-10-01T00:00:00.000Z', completed_at: null, cancelled_at: null,
}
const detail = {
  ...mission, events: [], revisions: [], parent: null,
  assignments: [{
    id: 'a1', role: 'executor', status: 'delivered', target_bot_id: BOT_ID, turn_status: null, turn_error: null,
    follow_up_of: null, resume_at: null, created_at: '2026-10-01T00:00:00.000Z', completed_at: null,
  }],
}
const bot = {
  id: BOT_ID, name: 'mission-exec', project_id: 'p1', kind: 'claude', identity: null, model: null, effort: null, args: [], env: {},
  autostart: false, inject_hooks: true, auto_approve: false, managed_by: 'user', parent_bot_id: null, primary: false, primary_position: 0, cwd: null,
}

const route = (req: FakeRequest) => {
  if (req.path.includes('/missions/m1')) return detail
  if (req.path.includes('/projects/p1/missions')) return [mission]
  return undefined
}

beforeEach(() => {
  fakeApi(route)
  useStore.setState({ missionsSupported: true, missions: { p1: [mission] }, missionDetail: { m1: detail }, bots: [bot] } as never)
})

test('任務卡顯示 bot 名字而不是 id', async () => {
  await mount(<MissionsBar projectId="p1" />)
  await settle(200)
  const actors = [...document.querySelectorAll('.mission-actors .mission-who')].map((e) => e.textContent!)
  const asgs = document.querySelector<HTMLElement>('.mission-asgs .mission-who')!
  assert.ok(actors.length > 0 && actors.every((t) => t.includes('mission-exec')), `角色列：${actors}`)
  assert.ok(asgs.textContent!.includes('mission-exec'), `交辦列：${asgs.textContent}`)
  assert.ok(!document.querySelector('.mission-card')!.textContent!.includes('01HZZZ'), '卡片上不能出現 bot id')
  assert.equal(asgs.getAttribute('title'), BOT_ID, '交辦列的 title 留著 id 方便對')
})

test('對不到名字（bot 已刪）時退回原字串', async () => {
  useStore.setState({ bots: [] } as never)
  await mount(<MissionsBar projectId="p1" />)
  await settle(200)
  const asgs = document.querySelector<HTMLElement>('.mission-asgs .mission-who')!
  assert.equal(asgs.textContent, BOT_ID)
})
