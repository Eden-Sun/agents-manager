/**
 * 任務卡「取消任務」走 ConfirmDialog（#1130）：原生 confirm() 在手機預覽的 iframe 裡不會顯示（沒有 allow-modals），
 * 按了什麼都不發生；長任務文字也會被整段塞進系統框。
 */
import test, { after, afterEach, before, beforeEach } from 'node:test'
import assert from 'node:assert/strict'
import { click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import type { FakeRequest } from '../testing/domHarness'
import { useStore } from '../store/store'
import { MissionsBar } from './MissionsBar'

afterEach(unmountAll)
before(setupDom)
after(teardownDom)

const longText = '任'.repeat(200)
const mission = {
  id: 'm1', project_id: 'p1', client_request_id: 'r1', text: longText, delivery_mode: 'push_main', executor_kind: 'claude',
  on_5h_limit: 'wait', max_rounds: 2, rounds_used: 0, paused_reason: null, paused_detail: null, result_summary: null,
  parent_mission_id: null, status: 'open', phase: 'executing', created_at: '2026-10-01T00:00:00.000Z',
  updated_at: '2026-10-01T00:00:00.000Z', completed_at: null, cancelled_at: null,
}
const detail = { ...mission, events: [], assignments: [], revisions: [], parent: null }

const route = (req: FakeRequest) => {
  if (req.method === 'POST' && req.path.includes('/missions/m1/cancel')) return { ...mission, status: 'cancelled', cancelled_at: '2026-10-01T01:00:00.000Z', phase: 'cancelled' }
  if (req.path.includes('/missions/m1')) return detail
  if (req.path.includes('/projects/p1/missions')) return [mission]
  return undefined
}

const cancelButton = () => [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent === '取消任務')!

let realConfirm: typeof globalThis.confirm
beforeEach(() => {
  realConfirm = globalThis.confirm
  globalThis.confirm = () => {
    throw new Error('不該呼叫原生 confirm')
  }
  useStore.setState({ missionsSupported: true, missions: { p1: [mission] }, missionDetail: { m1: detail } } as never)
})
afterEach(() => {
  globalThis.confirm = realConfirm
})

test('取消任務走 ConfirmDialog：長文字有截斷、按「先不要」不送請求，按確認才送', async () => {
  const requests = fakeApi(route)
  await mount(<MissionsBar projectId="p1" />)
  await settle(200)
  await click(cancelButton())
  const backdrop = document.querySelector('.confirm-backdrop')
  assert.ok(backdrop, '要出現確認框')
  assert.ok(backdrop!.textContent!.length < 200, '長任務文字要截斷，不能整段塞進框裡')

  await click([...document.querySelectorAll<HTMLButtonElement>('.confirm-actions button')][0])
  assert.equal(requests.filter((r) => r.method === 'POST').length, 0, '按取消不送請求')
  assert.equal(document.querySelector('.confirm-backdrop'), null, '按取消要關掉框')

  await click(cancelButton())
  await click([...document.querySelectorAll<HTMLButtonElement>('.confirm-actions button')].at(-1)!)
  await until(() => requests.some((r) => r.method === 'POST' && r.path.includes('/missions/m1/cancel')), '取消請求要送出')
})
