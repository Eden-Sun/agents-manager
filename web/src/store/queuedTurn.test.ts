import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Message, Project, Turn } from '../api/types.ts'

const { useStore } = await import('./store.ts')
const { queuedSendFor } = await import('./queuedSend.ts')
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

const bot = { id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude' } as Bot
const project = { id: 'p1', path: '/p1', label: 'p1', host: 'local' } as Project
const idleQueuedTurn = (extra: Partial<Turn> = {}) =>
  ({
    id: 't1',
    conversation_id: 'c1',
    run_id: 'r1',
    bot_id: 'b1',
    origin: 'web',
    status: 'queued',
    delivery: 'pending',
    unverified: false,
    autoResend: true,
    awaitsStart: false,
    awaitsIdle: true,
    startError: null,
    client_request_id: 'cr1',
    created_at: '2026-09-30T00:00:00Z',
    completed_at: null,
    ...extra,
  }) as Turn

function seed() {
  reset()
  useStore.setState({
    bots: [bot],
    projects: [project],
    runs: { b1: { id: 'r1', state: 'running', agent_status: 'working' } as never },
    turns: {},
    messages: {},
    drafts: {},
    notices: [],
    queuedSends: {},
  })
}

test('busy composer send asks daemon to queue and the selector projects its turn and user message', async () => {
  seed()
  routeDaemon((req) => {
    if (req.path.endsWith('/prompt')) return json({ turn_id: 't1', message_id: 'm1', delivery: 'queued' })
    if (req.path.includes('/messages'))
      return json({
        messages: [
          {
            id: 'm1',
            conversation_id: 'c1',
            turn_id: 't1',
            bot_id: 'b1',
            role: 'user',
            content: '跑完整測試',
            source: 'web',
            attachments_json: JSON.stringify([{ id: 'a1', name: 'test.png', mime: 'image/png', size: 3, path: '/a1' }]),
          },
        ],
        turns: [{ id: 't1', bot_id: 'b1', run_id: 'r1', status: 'queued', delivery: 'pending', awaits_idle: 1, created_at: '2026-09-30T00:00:00Z' }],
        has_more: false,
      })
    return json({})
  })
  const ok = await useStore.getState().sendPrompt('b1', '跑完整測試', ['a1'], false, false, undefined, true)
  assert.equal(ok, true, JSON.stringify({ requests, notices: useStore.getState().notices }))
  const prompt = requests.find((request) => request.path.endsWith('/prompt'))
  assert.ok(prompt)
  assert.equal((prompt.body as { queue_if_busy?: boolean }).queue_if_busy, true)
  assert.deepEqual(queuedSendFor(useStore.getState(), 'b1'), { turnId: 't1', text: '跑完整測試', attachments: ['a1'] })
  assert.equal(useStore.getState().turns.b1.t1.awaitsIdle, true)
  assert.equal(useStore.getState().turns.b1.t1.awaitsStart, false)
})

test('queue_slot_taken leaves the attempted text in the draft and shows the one-slot conflict', async () => {
  seed()
  useStore.setState({ drafts: { 'bot:b1': '正在編輯' } })
  routeDaemon((req) =>
    req.path.endsWith('/prompt')
      ? json({ error: 'conflict', reason: 'queue_slot_taken', turn_id: 'existing' }, 409)
      : req.path.endsWith('/state')
        ? json({ error: 'intentional state-read failure' }, 502)
        : json({}),
  )
  const ok = await useStore.getState().sendPrompt('b1', '第二則文字', ['a1'], false, false, undefined, true)
  assert.equal(ok, false)
  assert.equal(useStore.getState().drafts['bot:b1'], '第二則文字\n正在編輯')
  assert.ok(useStore.getState().notices.some((notice) => notice.text.includes('已有一則訊息排隊中') && notice.text.includes('1 個附件')))
})

test('withdrawing an awaits_idle turn restores the server-returned text before the current draft', async () => {
  seed()
  const message = {
    id: 'm1',
    conversation_id: 'c1',
    turn_id: 't1',
    bot_id: 'b1',
    role: 'user',
    content: 'cached text',
    source: 'web',
    incomplete: false,
    group_id: null,
    attachments: [{ id: 'a1', name: 'image.png', mime: 'image/png', size: 3, path: '/a1' }],
    relay_from: null,
    terminal_snapshot: null,
    created_at: '2026-09-30T00:00:00Z',
  } as Message
  useStore.setState({
    turns: { b1: { t1: idleQueuedTurn() } },
    messages: { b1: [message] },
    drafts: { 'bot:b1': 'typed while queued' },
  })
  routeDaemon((req) => {
    if (req.path.endsWith('/withdraw')) return json({ text: 'authoritative text', attachments: ['a1', 'a2'] })
    if (req.path.includes('/messages'))
      return json({
        messages: [{ id: 'm1', turn_id: 't1', role: 'user', content: 'authoritative text', source: 'web', attachments: [] }],
        turns: [{ id: 't1', bot_id: 'b1', run_id: 'r1', status: 'failed', delivery: 'failed', awaits_idle: 1 }],
        has_more: false,
      })
    return json({})
  })
  await useStore.getState().withdrawQueuedSend('b1')
  assert.ok(requests.some((request) => request.method === 'POST' && request.path.endsWith('/turns/t1/withdraw')))
  assert.equal(useStore.getState().drafts['bot:b1'], 'authoritative text\ntyped while queued')
  assert.equal(useStore.getState().turns.b1.t1.status, 'failed')
  assert.ok(useStore.getState().notices.some((notice) => notice.text.includes('2 個附件')))
})

test('first bootstrap migrates a leftover legacy in-memory row through /prompt once', async () => {
  seed()
  // Simulate a HMR-preserved state object from the pre-#733 bundle.
  useStore.setState({ queuedSends: { b1: { text: 'legacy row', attachments: [] } as never } })
  routeDaemon((req) => {
    if (req.path.endsWith('/session')) return json({ token: 't' })
    if (req.path.endsWith('/prompt')) return json({ turn_id: 'migrated', message_id: 'm1', delivery: 'queued' })
    // Keep this test's module-level appliedStateSeq unchanged for other store tests sharing the same worker.
    if (req.path.endsWith('/state')) return json({ error: 'intentional state-read failure' }, 502)
    return json({})
  })
  await useStore.getState().bootstrap()
  const prompts = requests.filter((request) => request.path.endsWith('/prompt'))
  assert.equal(prompts.length, 1)
  assert.equal((prompts[0].body as { queue_if_busy?: boolean }).queue_if_busy, true)
  assert.equal((prompts[0].body as { text?: string }).text, 'legacy row')
  useStore.setState({ projects: [], bots: [], runs: {}, turns: {}, messages: {}, socket: 'closed' })
})
