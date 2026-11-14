/**
 * claude-api 換模型後舊對話接續不了（daemon 409 `api_model_changed`）：store 要開確認框（不是通用的「啟動失敗」），
 * 而且「換回原模型」「清除對話」兩個選項真的會打對的 API。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import { requests, reset, routeDaemon } from './storeEnv.harness.ts'

const { useStore } = await import('./store.ts')

const json = (body: unknown, status: number) => new Response(JSON.stringify(body), { status })
const conflict = {
  error: 'api_model_changed',
  reason: 'api_model_changed',
  checkpoint_model: 'claude-opus-5-5',
  current_model: 'claude-fable-5-2',
}
const expected = { botId: 'b1', checkpointModel: 'claude-opus-5-5', currentModel: 'claude-fable-5-2' }

function seed() {
  reset()
  useStore.setState({ apiModelConflict: null, notices: [] })
}

test('startBot 收到 409 api_model_changed：開確認框，不跳「啟動失敗」', async () => {
  seed()
  routeDaemon((req) => (req.path.endsWith('/start') ? json(conflict, 409) : json({}, 200)))
  await useStore.getState().startBot('b1')
  assert.deepEqual(useStore.getState().apiModelConflict, expected)
  assert.equal(useStore.getState().notices.length, 0, '不該再跳通用錯誤')
})

test('送 prompt 收到 409 api_model_changed：同一個確認框（不是 502、不是通用錯誤）', async () => {
  seed()
  useStore.setState({ bots: [{ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude-api', identity: null, model: 'claude-fable-5-2' } as never] })
  routeDaemon((req) => (req.path.endsWith('/prompt') ? json(conflict, 409) : json({}, 200)))
  const sent = await useStore.getState().sendPrompt('b1', 'hi')
  assert.equal(sent, false)
  assert.deepEqual(useStore.getState().apiModelConflict, expected)
  assert.equal(useStore.getState().notices.filter((n) => n.kind === 'error').length, 0)
})

test('換回原模型：PATCH 回 opus、再啟動，確認框關掉', async () => {
  seed()
  useStore.setState({ apiModelConflict: expected })
  routeDaemon((req) => (req.method === 'PATCH' ? json({ needs_restart: false }, 200) : json({ run_id: 'r2' }, 200)))
  await useStore.getState().resolveApiModelConflict('revert')
  const patch = requests.find((r) => r.method === 'PATCH')
  assert.deepEqual(patch?.body, { model: 'claude-opus-5-5' })
  assert.ok(requests.some((r) => r.method === 'POST' && r.path.endsWith('/start')), '換回後要重新啟動')
  assert.equal(useStore.getState().apiModelConflict, null)
})

test('清除對話：先查 fresh target、退役 session，再從新模型啟動', async () => {
  seed()
  useStore.setState({ apiModelConflict: expected })
  routeDaemon((req) => {
    if (req.method === 'GET' && req.path.endsWith('/api_fresh_target')) return json({ availability: 'available', session_id: 's1', generation: 2 }, 200)
    return json({ result: 'cleared' }, 200)
  })
  await useStore.getState().resolveApiModelConflict('clear')
  const retire = requests.find((r) => r.method === 'POST' && r.path.endsWith('/api-session/retire'))
  assert.deepEqual(retire?.body, { session_id: 's1', generation: 2 })
  assert.ok(requests.some((r) => r.method === 'POST' && r.path.endsWith('/start')))
  assert.equal(useStore.getState().apiModelConflict, null)
})
