/**
 * issue #539：daemon 對已停用的模型別名會換掉再存並回 `remapped`（API.md「停用模型的回應」）。
 * 前端以前完全沒讀這一欄，於是使用者不知道自己選的沒生效，設定面板還一直顯示那個停用值。
 */
import test from 'node:test'
import assert from 'node:assert/strict'
import './storeEnv.harness.ts'
import { pruneSaved, type BotFormBase } from '../components/botSettingsForm.ts'
import { HIDDEN_MODELS, MODEL_OPTIONS, canonicalModel, type Bot, type Project } from '../api/types.ts'

const { useStore } = await import('./store.ts')

const bot = (over: Partial<Bot> = {}) => ({ id: 'b1', name: 'b1', project_id: 'p1', kind: 'claude', identity: null, model: 'sonnet', ...over }) as Bot
const project = () => ({ id: 'p1', label: 'p', path: '/p', host: 'local' }) as Project
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

/**
 * 自己裝 `fetch`（不走 harness 的 `routeDaemon`）：整套一起跑時，別的測試檔晚到的 `reset()` 會把共用的
 * route 洗回預設，這裡就收到 200 `{}`、斷言莫名其妙紅。跑完一定還原，別把全域留給下一個檔案。
 */
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

/** daemon：PATCH 回 remapped，之後的 `/api/state` 給替換後的值（daemon 存的就是那個）。 */
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
  assert.deepEqual(res, { needsRestart: false, remappedModel: { from: 'opus', to: applied } })
  assert.equal(patched, 1)

  const said = useStore.getState().notices.map((n) => n.text)
  assert.equal(said.length, 1, `只講一次，實際 ${said.length} 則`)
  assert.match(said[0], /opus/)
  assert.match(said[0], new RegExp(applied))

  // 面板把 daemon 實際採用的值記進 `saved`，`pruneSaved` 才追得上（記送出的 `opus` 會永遠追不上，
  // 欄位就一直顯示一個沒在用的模型、還算不出 dirty）。
  const stored: BotFormBase = { name: 'b1', model: applied, effort: null, fast: false, persona: null, identity: null, instruction_files: null }
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
  assert.deepEqual(res, { needsRestart: true, remappedModel: null })
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
