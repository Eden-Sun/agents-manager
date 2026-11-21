import test from 'node:test'
import assert from 'node:assert/strict'
import './storeEnv.harness.ts'
import { pruneSaved, type BotFormBase } from '../components/botSettingsForm.ts'
import { HIDDEN_MODELS, MODEL_OPTIONS, canonicalModel, type Bot, type Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')

const bot = (over: Partial<Bot> = {}) => ({ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', identity: null, model: 'sonnet', ...over }) as Bot
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

const g = globalThis as unknown as Record<string, unknown>
async function withDaemon<T>(handler: (path: string, method: string) => Response, body: () => Promise<T>): Promise<T> {
  const saved = g.fetch
  const sent: { path: string; method: string }[] = []
  g.fetch = async (input: string, init?: { method?: string }) => {
    const path = String(input).split('?')[0]
    const method = init?.method ?? 'GET'
    sent.push({ path, method })
    return handler(path, method)
  }
  try {
    return await body()
  } finally {
    g.fetch = saved
    patched = sent.filter((r) => r.method === 'PATCH').length
  }
}
let patched = 0

function daemonThatRemaps(applied: string) {
  useStore.setState({ projects: [project()], bots: [bot()], notices: [], busy: {} })
  return (path: string, method: string) => {
    if (path === '/api/session') return json({ token: 't' })
    if (path === '/api/bots/b1' && method === 'PATCH') {
      return json({ needs_restart: false, remapped: { model: { from: 'opus', to: applied } } })
    }
    if (path === '/api/state') {
      return json({ daemon_seq: 1, projects: [{ id: 'p1', path: '/p', bots: [{ id: 'b1', name: 'b1', kind: 'claude', model: applied }] }] })
    }
    return json({})
  }
}

test('#539：被換掉時回報實際採用的值，並講一句；面板記的也是那個值', async () => {
  const applied = canonicalModel('claude', 'opus')
  assert.equal(applied, 'claude-opus-5-5')

  const res = await withDaemon(daemonThatRemaps(applied), () => useStore.getState().patchBot('b1', { model: 'opus' }))
  assert.deepEqual(res, { needsRestart: false, deferred: false, remappedModel: { from: 'opus', to: applied } })
  assert.equal(patched, 1)

  const said = useStore.getState().notices.map((n) => n.text)
  assert.equal(said.length, 1, `只講一次，實際 ${said.length} 則`)
  assert.match(said[0], /opus/)
  assert.match(said[0], new RegExp(applied))

  const stored: BotFormBase = { name: 'b1', model: applied, effort: null, fast: false, persona: null, identity: null }
  assert.deepEqual(pruneSaved({ model: applied }, stored), {}, '記實際採用的值：放得掉')
  assert.deepEqual(pruneSaved({ model: 'opus' }, stored), { model: 'opus' }, '記送出的值：永遠追不上')
})

test('#539：沒有 remapped 就照舊，不要無中生有一則通知', async () => {
  useStore.setState({ projects: [project()], bots: [bot()], notices: [], busy: {} })
  const res = await withDaemon(
    (path) =>
      path === '/api/state'
        ? json({ daemon_seq: 1, projects: [{ id: 'p1', path: '/p', bots: [{ id: 'b1', name: 'b1', kind: 'claude', model: 'fable' }] }] })
        : json({ needs_restart: true }),
    () => useStore.getState().patchBot('b1', { model: 'fable' }),
  )
  assert.deepEqual(res, { needsRestart: true, deferred: false, remappedModel: null })
  assert.deepEqual(useStore.getState().notices, [])
})

test('#539：前端自己的清單不再放停用別名', () => {
  for (const [kind, ids] of Object.entries(MODEL_OPTIONS)) {
    for (const id of ids) {
      assert.equal(canonicalModel(kind as Bot['kind'], id), id, `${kind} 的 ${id} 是停用別名，要寫替換後的正式 id`)
    }
  }
})

test('#550：Codex 5.6 bots map to GPT-6 and the retired ids stay hidden', () => {
  assert.equal(canonicalModel('codex', 'gpt-5.6-sol'), 'gpt-6-sol')
  assert.equal(canonicalModel('codex', 'gpt-5.6-terra'), 'gpt-6-sol')
  assert.equal(canonicalModel('codex', 'gpt-5.6-luna'), 'gpt-6-luna')
  assert.deepEqual(MODEL_OPTIONS.codex, ['gpt-6-luna', 'gpt-6-sol', 'gpt-6-astra'])
  assert.ok(['gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna'].every((id) => HIDDEN_MODELS.includes(id)))
})
