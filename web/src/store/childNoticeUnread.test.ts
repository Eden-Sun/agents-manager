import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, routeDaemon } from './storeEnv.harness.ts'
import { resetIdleEdges, resetTurnCompletions } from './unread.ts'
import { dispatchFrameForTest, useStore } from './store.ts'
import type { Bot, Project } from '../api/types.ts'

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null, herdr_session: null }) as Bot
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project

function seed() {
  reset()
  resetIdleEdges()
  resetTurnCompletions()
  routeDaemon(() => json({ messages: [], turns: [], has_more: false }))
  useStore.setState({
    projects: [project()],
    bots: [bot('b1')],
    connected: true,
    runs: {},
    turns: {},
    messages: { b1: [] },
    loadedBots: {},
    botUnread: {},
    groupUnread: {},
    notices: [],
    selectedBotId: null,
    selectedProjectId: null,
  })
}

const turn = (id: string, status: string, clientRequestId: string | null) => ({
  id,
  conversation_id: 'c',
  run_id: 'r1',
  origin: 'web',
  status,
  delivery: 'ok',
  client_request_id: clientRequestId,
  created_at: '2026-10-05T10:00:00Z',
  completed_at: status === 'in_flight' ? null : '2026-10-05T10:00:30Z',
})

const reply = (id: string, turnId: string, keepWarm: boolean) => ({
  id,
  conversation_id: 'c',
  turn_id: turnId,
  role: 'assistant',
  content: '目前沒有新進度',
  source: 'hook',
  created_at: '2026-10-05T10:00:20Z',
  ...(keepWarm ? { keep_warm: true } : {}),
})

const status = (agent: 'working' | 'idle', extra: Record<string, unknown> = {}) => ({
  type: 'bot_status',
  data: { bot_id: 'b1', run: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: agent, ...extra } },
})

/** 一個完整回合：in_flight → working → 回覆 → completed → idle（hook 順序）。 */
function runTurn(turnId: string, crid: string | null, keepWarm: boolean) {
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn(turnId, 'in_flight', crid) } })
  dispatchFrameForTest(status('working'))
  dispatchFrameForTest({ type: 'message_added', data: { bot_id: 'b1', message: reply(`m-${turnId}`, turnId, keepWarm) } })
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn(turnId, 'completed', crid) } })
  dispatchFrameForTest(status('idle'))
}

for (const crid of ['child-done:child:t', 'child-blocked:child:t']) {
  test(`${crid} 完成事件／assistant／idle 不增加未讀；普通回合仍增加`, () => {
    seed()
    runTurn('t1', crid, false)
    assert.equal(useStore.getState().botUnread.b1 ?? 0, 0)
    assert.equal(useStore.getState().groupUnread.p1 ?? 0, 0)
    runTurn('t2', 'web-1', false)
    assert.equal(useStore.getState().botUnread.b1, 1)
  })
  test(`${crid} 只有 working→idle 備援也不增加未讀`, () => {
    seed()
    dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn('t1', 'in_flight', crid) } })
    dispatchFrameForTest(status('working'))
    dispatchFrameForTest(status('idle'))
    assert.equal(useStore.getState().botUnread.b1 ?? 0, 0)
  })
}

test('回合 metadata 還沒載入時，從通知訊息配對回覆仍不增加未讀', () => {
  seed()
  dispatchFrameForTest({ type: 'message_added', data: { bot_id: 'b1', message: {
    id: 'notice', conversation_id: 'c', turn_id: 't1', role: 'user', source: 'web', relay_from: 'child',
    content: '[daemon 自動通知，不是 bot_request]子 agent sc1779 已完成一個回合。', created_at: '2026-10-05T10:00:00Z',
  } } })
  dispatchFrameForTest({ type: 'message_added', data: { bot_id: 'b1', message: reply('a1', 't1', false) } })
  assert.equal(useStore.getState().botUnread.b1 ?? 0, 0)
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn('t1', 'completed', null) } })
  assert.equal(useStore.getState().botUnread.b1 ?? 0, 0)
})
