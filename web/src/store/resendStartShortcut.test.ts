import test from 'node:test'
import assert from 'node:assert/strict'
import { reset, requests, routeDaemon } from './storeEnv.harness.ts'
import type { Bot, Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

const RUN = { id: 'r1', bot_id: 'b1', state: 'running', agent_status: 'unknown', started_at: '2026-11-19T10:00:00Z', host: 'local' }

function daemon(start: () => Response) {
  let running = false
  routeDaemon((req) => {
    if (req.path === '/api/state') {
      const bots = [{ id: 'b1', project_id: 'p1', name: 'b1', kind: 'openai-api', parent_id: null, model: 'gpt-6-luna', effort: 'medium' }]
      return json({ daemon_seq: 1, projects: [{ id: 'p1', label: 'p1', path: '/p1', host: 'local', bots }], bots, runs: running ? [RUN] : [], turns: [] })
    }
    if (req.path === '/api/bots/b1/start') {
      const res = start()
      if (res.status === 200) running = true
      return res
    }
    if (req.path === '/api/bots/b1/prompt') return json({ request_id: 'q', turn_id: 't', message_id: 'm', delivery: 'pending' }, 202)
    return json({})
  })
}

function openAiBot() {
  reset()
  useStore.setState({
    bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind: 'openai-api' } as Bot],
    projects: [{ id: 'p1', path: '/p1', label: 'p1', host: 'local' } as Project],
    runs: {},
  } as never)
}

const promptCalls = () => requests.filter((r) => r.method === 'POST' && r.path === '/api/bots/b1/prompt')
const startCalls = () => requests.filter((r) => r.method === 'POST' && r.path === '/api/bots/b1/start')

test('前端以為停著、daemon 已經 running：409 api_run_already_active 不是錯誤，只送一次 prompt、不顯示啟動失敗', async () => {
  openAiBot()
  daemon(() => json({ error: 'conflict', reason: 'api_run_already_active', run_id: 'r1' }, 409), )
  const ok = await useStore.getState().sendPrompt('b1', 'hi', [], false, true)
  assert.equal(ok, true)
  assert.equal(promptCalls().length, 1, '只送一次')
  const crids = promptCalls().map((r) => (r.body as { client_request_id?: string }).client_request_id)
  assert.equal(crids.length, 1)
  const texts = useStore.getState().notices.map((n) => n.text)
  assert.ok(!texts.some((t) => /啟動失敗/.test(t)), JSON.stringify(texts))
  assert.equal(startCalls().length, 1)
})

test('捷徑 start 成功後 store 立刻有這顆 bot 的 run，下一次送出不再 start', async () => {
  openAiBot()
  daemon(() => json({ run_id: 'r1', resumed: false }), )
  await useStore.getState().sendPrompt('b1', 'first', [], false, true)
  assert.equal(useStore.getState().runs['b1']?.state, 'running', 'start 之後 store 有 run')
  await useStore.getState().sendPrompt('b1', 'second', [], false, true)
  assert.equal(startCalls().length, 1, '第二次送出不再 start')
  assert.equal(promptCalls().length, 2)
})

test('真的無法確認（502）仍是失敗：不送 prompt、顯示啟動失敗', async () => {
  openAiBot()
  daemon(() => json({ error: 'api_native_start_unconfirmed' }, 502), )
  const ok = await useStore.getState().sendPrompt('b1', 'hi', [], false, true)
  assert.equal(ok, false)
  assert.equal(promptCalls().length, 0, '沒有確認就不送')
  const texts = useStore.getState().notices.map((n) => n.text)
  assert.ok(texts.some((t) => /啟動失敗/.test(t)), JSON.stringify(texts))
  assert.ok(texts.some((t) => /OpenAI/.test(t) && !/claude/.test(t)), 'openai bot 的錯誤句不寫 claude-api：' + JSON.stringify(texts))
})
