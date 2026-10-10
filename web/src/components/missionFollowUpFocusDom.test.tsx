/** 成果卡的追問與追加修改要接住表單焦點，關閉後還原到開啟入口。 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { act, fakeApi, mount, settle, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { MissionsBar } from './MissionsBar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const mission = {
  id: 'm1', project_id: 'p1', client_request_id: 'r1', text: '整理文件', delivery_mode: 'push_main', executor_kind: 'claude',
  on_5h_limit: 'wait', max_rounds: 2, rounds_used: 2, paused_reason: null, paused_detail: null, result_summary: '已整理',
  parent_mission_id: null, status: 'done', phase: 'done', created_at: '2026-10-01T00:00:00.000Z',
  updated_at: '2026-10-01T00:05:00.000Z', completed_at: '2026-10-01T00:05:00.000Z', cancelled_at: null,
}
const detail = { ...mission, assignments: [], revisions: [], parent: null, events: [] }
const route = (req: FakeRequest) => {
  if (req.path.includes('/missions/m1')) return detail
  if (req.path.includes('/projects/p1/missions')) return [mission]
  return undefined
}

beforeEach(() => {
  fakeApi(route)
  useStore.setState({
    missionsSupported: true,
    missions: { p1: [mission] },
    missionDetail: { m1: detail },
    missionLoading: {},
    missionLoadErrors: {},
    askMission: async () => true,
    reviseMission: async () => 'm2',
  } as never)
})

async function openDoneRow() {
  await mount(<MissionsBar projectId="p1" />)
  await settle(200)
  const doneToggle = [...document.querySelectorAll<HTMLButtonElement>('button.mission-done-head')]
    .find((button) => button.textContent?.includes('已完成任務'))!
  await act(() => doneToggle.click())
  const rowToggle = document.querySelector<HTMLButtonElement>('.mission-done-row-head')!
  await act(() => rowToggle.click())
  await settle(200)
}

async function clickButton(label: string) {
  const button = [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((candidate) => candidate.textContent?.trim() === label)!
  await act(() => button.click())
}

test('展開成果卡時不搶走焦點', async () => {
  await openDoneRow()
  const askButton = [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((button) => button.textContent?.trim() === '追問')!
  assert.notEqual(document.activeElement, askButton)
})

test('追問開啟時聚焦輸入框，取消後回到追問按鈕', async () => {
  await openDoneRow()
  await clickButton('追問')
  assert.equal(document.activeElement, document.querySelector('textarea[aria-label="追問內容"]'))
  await clickButton('取消')
  assert.equal(document.activeElement, [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((button) => button.textContent?.trim() === '追問'))
})

test('追加修改開啟時聚焦輸入框，取消後回到追加修改按鈕', async () => {
  await openDoneRow()
  await clickButton('追加修改')
  assert.equal(document.activeElement, document.querySelector('textarea[aria-label="追加修改內容"]'))
  await clickButton('取消')
  assert.equal(document.activeElement, [...document.querySelectorAll<HTMLButtonElement>('button')]
    .find((button) => button.textContent?.trim() === '追加修改'))
})
