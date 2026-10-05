/**
 * 保溫回覆不亮未讀（SPEC §6.5k）：保溫回合是 daemon 代送的，不管走 `message_added`、`turn_updated` 還是 working→idle 邊緣，
 * 都不能讓 `botUnread` 長出一筆；一般回合照樣記（對照組）。`client_request_id` 新前綴 `keep-warm:` 與舊資料的 `keepalive:` 都認。
 */
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

test('一般回合照記未讀（對照組）', () => {
  seed()
  runTurn('t1', 'web-1', false)
  assert.equal(useStore.getState().botUnread.b1, 1)
})

test('保溫回覆（新前綴 keep-warm:）不亮未讀', () => {
  seed()
  runTurn('t1', 'keep-warm:anchor1', true)
  assert.equal(useStore.getState().botUnread.b1, undefined)
  assert.equal(useStore.getState().groupUnread.p1 ?? 0, 0)
})

test('保溫回覆（DB 舊前綴 keepalive:）也不亮；訊息沒帶 keep_warm 時靠回合前綴擋住', () => {
  seed()
  runTurn('t1', 'keepalive:anchor0', false)
  assert.equal(useStore.getState().botUnread.b1, undefined)
})

test('保溫之後接一個真的回合：只有真的那個記一筆', () => {
  seed()
  runTurn('t1', 'keep-warm:anchor1', true)
  runTurn('t2', 'web-2', false)
  assert.equal(useStore.getState().botUnread.b1, 1)
})

test('終端的 working→idle 邊緣補記：保溫回合在飛時也不記', () => {
  seed()
  dispatchFrameForTest({ type: 'turn_updated', data: { bot_id: 'b1', turn: turn('t1', 'in_flight', 'keep-warm:anchor1') } })
  dispatchFrameForTest(status('working'))
  dispatchFrameForTest(status('idle'))
  assert.equal(useStore.getState().botUnread.b1, undefined)
})

test('run 帶 keep_warm_replied_at／keep_warm_skip：進 store；送新 prompt 後 daemon 清成 null 也跟著清', () => {
  seed()
  dispatchFrameForTest(status('idle', { keep_warm_replied_at: '2026-10-05T10:00:30Z', keep_warm_skip: true, cache_kept_warm_at: '2026-10-05T10:00:30Z' }))
  const r = useStore.getState().runs.b1!
  assert.equal(r.keep_warm_replied_at, '2026-10-05T10:00:30Z')
  assert.equal(r.keep_warm_skip, true)
  assert.equal(r.cache_kept_warm_at, '2026-10-05T10:00:30Z')
  dispatchFrameForTest(status('idle', { keep_warm_replied_at: null, keep_warm_skip: false }))
  const after = useStore.getState().runs.b1!
  assert.equal(after.keep_warm_replied_at, null)
  assert.equal(after.keep_warm_skip, false)
})
