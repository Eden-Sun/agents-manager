/**
 * 快照與 WS 交錯：較舊的 HTTP 頁不准蓋掉請求期間已經套上的終態（#649、#650、#651）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import { resetIdleEdges, resetTurnCompletions } from './unread.ts'
import { composerState, dispatchFrameForTest, useStore } from './store.ts'
import { modelsKey } from './modelsCache.ts'
import type { Bot, Project, Run } from '../api/types.ts'

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const settle = () => new Promise((r) => setTimeout(r, 0))

const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null, herdr_session: null }) as Bot
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const run = (status: 'working' | 'idle'): Run =>
  ({ id: 'r1', bot_id: 'b1', state: 'running', agent_status: status }) as Run

function seed() {
  reset()
  resetIdleEdges()
  resetTurnCompletions()
  useStore.setState({
    projects: [project()],
    bots: [bot('b1')],
    connected: true,
    runs: { b1: run('idle') },
    turns: {},
    messages: { b1: [] },
    loadedBots: {},
    botUnread: {},
    groupUnread: {},
    notices: [],
    quota: {},
    models: {},
    modelsFailedAt: {},
    selectedBotId: null,
    selectedProjectId: null,
  })
}

function hang(match: (path: string) => boolean) {
  let release: (res: Response) => void = () => {}
  const gate = new Promise<Response>((resolve) => {
    release = resolve
  })
  routeDaemon((req) => (match(req.path) ? gate : json({})))
  return (body: unknown, status = 200) => release(json(body, status))
}

const rawTurn = (id: string, status: string) => ({
  id,
  conversation_id: 'c',
  run_id: 'r1',
  origin: 'web',
  status,
  delivery: 'ok',
  client_request_id: null,
  created_at: '2026-09-15T10:00:00Z',
  completed_at: status === 'in_flight' || status === 'queued' ? null : '2026-09-15T10:05:00Z',
})

test('loadMessages：請求期間 WS 已完成的回合，不被較舊頁蓋回 in_flight', async () => {
  seed()
  const release = hang((path) => path.includes('/bots/b1/messages'))
  const pending = useStore.getState().loadMessages('b1')
  await settle()
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: rawTurn('t1', 'completed') } })
  assert.equal(useStore.getState().turns.b1.t1.status, 'completed')
  release({ messages: [], turns: [rawTurn('t1', 'in_flight')], has_more: false })
  await pending
  assert.equal(useStore.getState().turns.b1.t1.status, 'completed')
  const cs = composerState(useStore.getState(), 'b1')
  assert.equal(cs.queued, false)
  assert.equal(cs.inFlightTurnId, null)
})

test('終端回合不拿上一筆群組回合的 id 去加群組未讀', async () => {
  seed()
  routeDaemon(() => json({ messages: [], turns: [], has_more: false }))
  dispatchFrameForTest({
    type: 'message_added',
    data: {
      bot_id: 'b1',
      message: {
        id: 'm1',
        conversation_id: 'c',
        turn_id: 't-g',
        role: 'user',
        content: 'hi',
        source: 'web',
        group_id: 'g1',
        created_at: '2026-09-15T10:00:00Z',
      },
    },
  })
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: rawTurn('t-g', 'completed') } })
  await settle()
  assert.equal(useStore.getState().groupUnread.p1, 1, '群組回合本身算一次')
  const edge = (status: 'working' | 'idle') =>
    dispatchFrameForTest({
      type: 'bot_status',
      data: { bot_id: 'b1', connected: true, run: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: status } },
    })
  edge('working')
  edge('idle')
  edge('working')
  edge('idle')
  await settle()
  await settle()
  assert.equal(useStore.getState().groupUnread.p1, 1, '兩個終端回合不再各 +1')
})

const quotaBody = (used: number) => ({
  kinds: {
    'claude:work': { five_hour: { used_pct: used, low: false, critical: false }, updated_at: 't', host: 'local', plan: 'max' },
    'codex:old': { five_hour: { used_pct: 10, low: false, critical: false }, updated_at: 't', host: 'local' },
  },
})

test('loadQuota：請求期間的 quota_updated 不被較舊快照蓋掉，沒更新的 key 仍用快照', async () => {
  seed()
  useStore.setState({
    quota: {
      'claude:work': { five_hour: { used_pct: 80, resets_at: null, observed_at: null, low: false, critical: false }, seven_day: null, fable: null, reset_credits: null, limit_hit: null, plan: 'max', updated_at: 't', stale: false, host: 'local' },
      'codex:gone': { five_hour: null, seven_day: null, fable: null, reset_credits: null, limit_hit: null, plan: null, updated_at: 't', stale: false, host: 'local' },
    },
  })
  const release = hang((path) => path === '/api/quota')
  const pending = useStore.getState().loadQuota()
  await settle()
  dispatchFrameForTest({
    type: 'quota_updated',
    data: { kind: 'claude:work', quota: { five_hour: { used_pct: 0, low: false, critical: false }, updated_at: 'new', host: 'local', plan: 'max' } },
  })
  assert.equal(useStore.getState().quota['claude:work']?.five_hour?.used_pct, 0)
  release(quotaBody(90))
  await pending
  assert.equal(useStore.getState().quota['claude:work']?.five_hour?.used_pct, 0, 'WS 的新讀數要留下')
  assert.equal(useStore.getState().quota['codex:old']?.five_hour?.used_pct, 10, '沒收到 frame 的 key 用快照')
  assert.equal(useStore.getState().quota['codex:gone'], undefined, '快照沒有、期間也沒 frame 的 key 要刪掉')
})

test('loadModels：較舊的失敗回應在 dropHostModels 之後不能寫 null', async () => {
  seed()
  const release = hang((path) => path.startsWith('/api/models'))
  const pending = useStore.getState().loadModels('claude', 'box', 'work')
  await settle()
  useStore.getState().dropHostModels('box')
  release({ error: 'unavailable' }, 503)
  const list = await pending
  const key = modelsKey('claude', 'box', 'work')
  assert.equal(list, null)
  assert.equal(useStore.getState().models[key], undefined, '過期失敗不能把快取寫成 null')
  assert.equal(useStore.getState().modelsFailedAt[key], undefined)
})

test('loadModels：較新的清單不能被較舊的成功回應蓋掉', async () => {
  seed()
  const gates: ((res: Response) => void)[] = []
  routeDaemon((req) => {
    if (!req.path.startsWith('/api/models')) return json({})
    return new Promise<Response>((resolve) => gates.push(resolve))
  })
  const first = useStore.getState().loadModels('claude', 'local', null)
  const second = useStore.getState().loadModels('claude', 'local', null)
  await settle()
  assert.equal(gates.length, 2)
  gates[1](json({ models: [{ id: 'new', display_name: 'new' }] }))
  assert.deepEqual((await second)?.map((m) => m.id), ['new'])
  gates[0](json({ models: [{ id: 'old', display_name: 'old' }] }))
  await first
  const key = modelsKey('claude', 'local', null)
  assert.deepEqual(useStore.getState().models[key]?.map((m) => m.id), ['new'])
})
