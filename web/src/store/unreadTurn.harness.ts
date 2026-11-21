import { reset, routeDaemon } from './storeEnv.harness.ts'
import { resetIdleEdges, resetTurnCompletions } from './unread.ts'
import { dispatchFrameForTest, useStore } from './store.ts'
import type { Bot, Project } from '../api/types.ts'

export const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

export const bot = (id: string) => ({ id, name: id, project_id: 'p1', kind: 'claude', identity: null, herdr_session: null }) as Bot
export const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project

export function seed() {
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

    notices: [],
    selectedBotId: null,
    selectedProjectId: null,
  })
}

export const turn = (id: string, status: string, clientRequestId: string | null) => ({
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

export const reply = (id: string, turnId: string, keepWarm: boolean) => ({
  id,
  conversation_id: 'c',
  turn_id: turnId,
  role: 'assistant',
  content: '目前沒有新進度',
  source: 'hook',
  created_at: '2026-10-05T10:00:20Z',
  ...(keepWarm ? { keep_warm: true } : {}),
})

export const status = (agent: 'working' | 'idle', extra: Record<string, unknown> = {}) => ({
  type: 'bot_status',
  data: { bot_id: 'b1', run: { id: 'r1', bot_id: 'b1', state: 'running', agent_status: agent, ...extra } },
})

export function runTurn(turnId: string, crid: string | null, keepWarm: boolean) {
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn(turnId, 'in_flight', crid) } })
  dispatchFrameForTest(status('working'))
  dispatchFrameForTest({ type: 'message_added', data: { bot_id: 'b1', message: reply(`m-${turnId}`, turnId, keepWarm) } })
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn(turnId, 'completed', crid) } })
  dispatchFrameForTest(status('idle'))
}
