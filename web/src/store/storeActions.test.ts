/**
 * store 動作的不變量：樂觀更新一定要收得回來，找不到的那一筆不可以拖垮整個功能。
 * 這些都不是純函式測得到的——它們的 bug 長在「送失敗之後沒有人善後」那一段。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Mission, MissionDetail, Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')

const bot = (id: string, extra: Partial<Bot> = {}) =>
  ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null, ...extra }) as Bot
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const json = (body: unknown, status: number) => new Response(JSON.stringify(body), { status })

function seed() {
  reset()
  useStore.setState({
    projects: [project()],
    bots: [bot('b1'), bot('b2')],
    botOrder: {},
    projectOrder: [],
    botUnread: {},
    queuedSends: {},
    drafts: {},
    notices: [],
    missions: {},
    missionDetail: {},
    missionLoadErrors: {},
    missionsSupported: true,
    selectedBotId: null,
    selectedProjectId: null,
  })
}

const settle = () => new Promise((r) => setTimeout(r, 10))

test('任務被清掉：收掉那一張卡，其他任務與「交給 AGM」不受影響', async () => {
  seed()
  const gone = { id: 'm1', project_id: 'p1' } as Mission
  useStore.setState({
    missions: { p1: [gone, { id: 'm2', project_id: 'p1' } as Mission] },
    missionDetail: { m1: { id: 'm1', project_id: 'p1' } as MissionDetail },
  })
  routeDaemon(() => json({ error: 'not_found', what: 'mission' }, 404))
  await useStore.getState().loadMission('m1')
  const s = useStore.getState()
  assert.equal(s.missionsSupported, true, '一筆找不到不等於這台 daemon 沒有群組任務')
  assert.equal(s.missionDetail.m1, undefined)
  assert.deepEqual(
    s.missions.p1.map((m) => m.id),
    ['m2'],
  )
  assert.match(s.missionLoadErrors.m1, /不在/)
})

test('SPA fallback 的 404（body 不是 daemon 的錯誤）仍然當成這台沒有群組任務', async () => {
  seed()
  routeDaemon(() => new Response('<!doctype html>', { status: 404 }))
  await useStore.getState().loadMission('m9')
  assert.equal(useStore.getState().missionsSupported, false)
})

test('交給 AGM 的回應在路上斷掉：再按一次沿用同一個 crid，不會開出第二筆任務', async () => {
  seed()
  const input = { text: '幫我修 lint', delivery_mode: 'push_main', executor_kind: 'claude', on_5h_limit: 'wait' } as const
  routeDaemon(() => json({ error: 'upstream', message: 'connection dropped' }, 502))
  assert.equal(await useStore.getState().startMission('p1', { ...input }), null)
  // 使用者看到「交給 AGM 失敗」才再按一次，中間一定隔了一段時間：拿時間當 crid 的寫法在這裡才現形。
  await settle()
  routeDaemon(() => json({ mission: { id: 'm1', project_id: 'p1' }, created: false }, 200))
  assert.equal(await useStore.getState().startMission('p1', { ...input }), 'm1')
  const crids = requests
    .filter((r) => r.method === 'POST')
    .map((r) => (r.body as { client_request_id?: string }).client_request_id)
  assert.equal(crids.length, 2)
  assert.ok(crids[0], 'store 要自己給 crid')
  assert.equal(crids[0], crids[1], '重送必須是同一個 crid，daemon 才回同一筆')
})
