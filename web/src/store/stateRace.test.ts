/**
 * 快照與 WS 交錯：較舊的 HTTP 頁不准蓋掉請求期間已經套上的終態（#649、#650、#651）。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import { resetIdleEdges, resetTurnCompletions } from './unread.ts'
import { composerState, dispatchFrameForTest, useStore } from './store.ts'
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
