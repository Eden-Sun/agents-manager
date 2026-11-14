/** claude-api 的快取時間：狀態列剩餘、冷提示（含重寫量與估價）、設定面板的 5 分鐘／1 小時。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, unmountAll, type FakeRequest } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { rawTransport } from '../api'
import { apiColdWarning, cacheView } from '../lib/composerCost'
import { ComposerCostHint } from './ComposerCostHint'
import { StatusCache } from './StatusCache'
import { ChatPanel } from './ChatPanel'
import { toRun } from '../api/normalize'

before(setupDom)
afterEach(async () => { await unmountAll(); resetStoreForTest() })
after(teardownDom)

const minAgo = (m: number) => new Date(Date.now() - m * 60_000).toISOString()

function seed(opts: { model?: string; ttl: 300 | 3600; last: string; agent?: string; fast?: boolean }) {
  useStore.setState({
    bots: [{ id: 'a1', name: 'api', project_id: 'p1', kind: 'claude-api', identity: null, model: opts.model ?? 'claude-opus-5-5', effort: 'high', fast: opts.fast ?? false, args: [], env: {}, autostart: false }],
    runs: { a1: { id: 'r1', bot_id: 'a1', state: 'running', agent_status: opts.agent ?? 'idle', started_at: minAgo(300), agent_status_since: minAgo(300), last_api_at: opts.last, cache_ttl_secs: opts.ttl, prompt_cache: null, status: null } },
    turns: {}, messages: {}, busy: {},
  } as never)
}

/** 最近一次呼叫讀到 10 萬 token（input 1k＋cache_read 90k＋cache_creation 9k）。 */
async function withLast<T>(fn: () => Promise<T>): Promise<T> {
  const orig = rawTransport.request.bind(rawTransport)
  rawTransport.request = (async (method: string, path: string, body?: unknown) => {
    if (path.includes('/api-usage')) {
      return {
        attempts: { total: 1, recorded: 1, reserved: 0, unknown: 0, failed: 0 }, cost_microusd: { recorded: 1, reserved: 0, unknown: 0 },
        tokens: { input: 1, output: 1, cache_read: 0, cache_creation: 0 }, cache_hit_rate: null, turns: [],
        last: { speed: 'standard', at: null, input_tokens: 1000, output_tokens: 5, cache_read_input_tokens: 90_000, cache_creation_input_tokens: 9000 },
      }
    }
    return orig(method as never, path, body)
  }) as typeof rawTransport.request
  try { return await fn() } finally { rawTransport.request = orig }
}
const warn = () => document.querySelector<HTMLElement>('.composer-cost')?.textContent ?? null

test('冷了：提示下一則約重寫多少 token 與約多少錢（5 分鐘寫入價）；不擋送出（沒有按鈕）', async () => {
  seed({ ttl: 300, last: minAgo(10) })
  await withLast(async () => {
    await mount(<ComposerCostHint botId="a1" />)
    await settle(50)
    assert.equal(warn(), '快取已冷：下一則約重寫 100K token（約 $0.50）')
    assert.equal(document.querySelectorAll('.composer-cost button').length, 0)
  })
})

test('1 小時 TTL 用 1 小時寫入價、fast 2 倍；70 分鐘前才算冷', async () => {
  seed({ model: 'claude-fable-5-2', ttl: 3600, last: minAgo(70), fast: true })
  await withLast(async () => {
    await mount(<ComposerCostHint botId="a1" />)
    await settle(50)
    assert.equal(warn(), '快取已冷：下一則約重寫 100K token（約 $3.20）') // 100k × $16/M × 2
  })
})

test('剩不到 1 分鐘也提示；還熱、或回合進行中不提示', async () => {
  seed({ ttl: 300, last: minAgo(4.5) })
  await withLast(async () => {
    await mount(<ComposerCostHint botId="a1" />)
    await settle(50)
    assert.match(warn() ?? '', /^快取剩不到 1 分鐘：下一則約重寫 100K token/)
    await unmountAll()
    seed({ ttl: 300, last: minAgo(1) })
    await mount(<ComposerCostHint botId="a1" />)
    await settle(50)
    assert.equal(warn(), null, '還剩約 4 分鐘')
    await unmountAll()
    seed({ ttl: 300, last: minAgo(10), agent: 'working' })
    await mount(<ComposerCostHint botId="a1" />)
    await settle(50)
    assert.equal(warn(), null, '回合進行中')
  })
})

test('apiColdWarning：不認得的模型只講 token 數；不知道重寫量講整段 context', () => {
  const cold = { state: 'cold' as const, mins: null, working: false, approx: false, rewriteTokens: 45_000 }
  assert.equal(apiColdWarning(cold, 'claude-x', 300, false), '快取已冷：下一則約重寫 45K token')
  assert.equal(apiColdWarning({ ...cold, rewriteTokens: null }, 'claude-opus-5-5', 300, false), '快取已冷：下一則會重寫整段 context')
  assert.equal(apiColdWarning(null, 'claude-opus-5-5', 300, false), null)
})

test('狀態列「快取」欄對 claude-api 也顯示剩餘時間（依這顆 bot 的 TTL）', async () => {
  seed({ ttl: 3600, last: minAgo(20) })
  await withLast(async () => {
    await mount(<StatusCache botId="a1" status={null} />)
    await settle(50)
    assert.match(document.querySelector('.sl-item[data-k="快取"] .sl-v')?.textContent ?? '', /^熱（剩 4\d 分）$/)
  })
  const input = { kind: 'claude-api', status: null, lastApiAt: minAgo(20), ttlSecs: 300, nowMs: Date.now(), working: false }
  assert.equal(cacheView(input)?.state, 'cold', '5 分鐘 TTL 過 20 分鐘就冷')
})


function seedChat(kind: 'claude-api' | 'claude') {
  useStore.setState({
    connected: true, defaultConnected: true, selectedBotId: 'c1',
    bots: [{ id: 'c1', name: 'c1', kind, project_id: 'pc', model: 'claude-opus-5-5', effort: 'high', fast: false, herdr_session: 'default' }],
    projects: [{ id: 'pc', path: '/tmp/cache-ttl', host: 'local' }],
    runs: { c1: toRun({ id: 'rc', bot_id: 'c1', state: 'running', agent_status: 'idle' }) },
    turns: { c1: {} }, drafts: { 'bot:c1': 'hello' }, messages: { c1: [] },
  } as never)
}
const promptBody = (requests: FakeRequest[]) => requests.filter((r) => r.method === 'POST' && /\/bots\/c1\/prompt$/.test(r.path)).map((r) => r.body as Record<string, unknown>)

test('claude-api 的輸入框有兩顆送出鍵：「快取 5 分」「快取 1 小時」，各自帶 cache_ttl；Enter 走 5 分鐘', async () => {
  seedChat('claude-api')
  const requests = fakeApi((req) => (req.method === 'POST' && /prompt$/.test(req.path) ? { turn_id: 't1', message_id: 'm1', delivery: 'ok' } : undefined))
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  const btns = [...document.querySelectorAll<HTMLButtonElement>('.send-btns .send-btn')]
  assert.deepEqual(btns.map((b) => b.textContent), ['送出（快取 5 分）', '送出（快取 1 小時）'])
  assert.match(btns[1]!.title, /寫入貴 60%/)
  await click(btns[1]!)
  await settle(50)
  assert.equal(promptBody(requests)[0]?.cache_ttl, '1h')

  useStore.setState({ drafts: { 'bot:c1': 'again' }, turns: { c1: {} } } as never)
  await settle(20)
  await click(document.querySelector<HTMLButtonElement>('.send-btns .send-btn[data-ttl="5m"]')!)
  await settle(50)
  assert.equal(promptBody(requests)[1]?.cache_ttl, '5m')

  // Enter＝5 分鐘（不沿用上次點的 1 小時）。
  useStore.setState({ drafts: { 'bot:c1': 'enter' }, turns: { c1: {} } } as never)
  await settle(20)
  const ta = document.querySelector<HTMLTextAreaElement>('textarea')!
  await act(async () => { ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true })) })
  await settle(50)
  assert.equal(promptBody(requests)[2]?.cache_ttl, '5m')
})

test('其他 kind 只有一顆送出鍵、不帶 cache_ttl', async () => {
  seedChat('claude')
  const requests = fakeApi((req) => (req.method === 'POST' && /prompt$/.test(req.path) ? { turn_id: 't1', message_id: 'm1', delivery: 'ok' } : undefined))
  await mount(<ChatPanel onOpenSidebar={() => {}} />)
  assert.equal(document.querySelectorAll('.send-btns').length, 0)
  const btn = document.querySelector<HTMLButtonElement>('.bot-composer .send-btn, .composer-box .send-btn')!
  await click(btn)
  await settle(50)
  assert.ok(!('cache_ttl' in (promptBody(requests)[0] ?? {})))
})
