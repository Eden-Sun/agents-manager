/** claude-api 的快取時間：狀態列剩餘、冷提示（含重寫量與估價）、設定面板的 5 分鐘／1 小時。 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
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

test('快取快過期也提示並提供壓縮按鈕；還熱、或回合進行中不提示', async () => {
  seed({ ttl: 300, last: minAgo(4.5) })
  await withLast(async () => {
    await mount(<ComposerCostHint botId="a1" />)
    await settle(50)
    assert.match(warn() ?? '', /^快取快過期，繼續工作就送出／可先壓縮：下一則約重寫 100K token/)
    assert.ok(document.querySelector('.composer-cost .compact-btn'), '提供手動壓縮按鈕')
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

test('手機版（393×852 / PHONE_QUERY）：claude-api 兩顆按鈕左右並排、DOM 結構分拆「送出」與「(分/時)」換行縮小寬度，各自帶 cache_ttl', async () => {
  const origMatch = window.matchMedia
  window.matchMedia = ((q: string) => ({
    matches: q.includes('max-width: 640px'),
    media: q,
    addEventListener() {},
    removeEventListener() {},
  })) as unknown as typeof window.matchMedia

  try {
    seedChat('claude-api')
    const requests = fakeApi((req) => (req.method === 'POST' && /prompt$/.test(req.path) ? { turn_id: 't1', message_id: 'm1', delivery: 'ok' } : undefined))
    await mount(<ChatPanel onOpenSidebar={() => {}} />)

    const sendBtnsGroup = document.querySelector<HTMLElement>('.send-btns')!
    assert.ok(sendBtnsGroup)
    assert.equal(sendBtnsGroup.getAttribute('role'), 'group')

    const btns = [...document.querySelectorAll<HTMLButtonElement>('.send-btns .send-btn')]
    assert.equal(btns.length, 2)

    // DOM 結構與 class 檢查：每顆包含 .send-btn-label，內有 .send-btn-action ("送出") 與 .send-btn-ttl ("(5 分)" / "(1 小時)")
    const labelWraps = [...document.querySelectorAll<HTMLElement>('.send-btns .send-btn-label')]
    assert.equal(labelWraps.length, 2)

    const actions = [...document.querySelectorAll<HTMLElement>('.send-btns .send-btn-action')]
    assert.deepEqual(actions.map((a) => a.textContent), ['送出', '送出'])

    const ttls = [...document.querySelectorAll<HTMLElement>('.send-btns .send-btn-ttl')]
    assert.deepEqual(ttls.map((t) => t.textContent), ['(5 分)', '(1 小時)'])

    assert.deepEqual(btns.map((b) => b.textContent), ['送出(5 分)', '送出(1 小時)'])
    assert.equal(btns[0]!.getAttribute('data-ttl'), '5m')
    assert.equal(btns[1]!.getAttribute('data-ttl'), '1h')
    assert.ok(btns[1]!.classList.contains('long'))

    // 點擊手機版 1 小時按鈕
    await click(btns[1]!)
    await settle(50)
    assert.equal(promptBody(requests)[0]?.cache_ttl, '1h')

    // 點擊手機版 5 分鐘按鈕
    useStore.setState({ drafts: { 'bot:c1': 'mobile-again' }, turns: { c1: {} } } as never)
    await settle(20)
    await click(btns[0]!)
    await settle(50)
    assert.equal(promptBody(requests)[1]?.cache_ttl, '5m')
  } finally {
    window.matchMedia = origMatch
  }
})

test('styles.css：桌面 .send-btns 上下排（column），手機 ≤640px 左右並排（row）且文字換行', () => {
  const css = readFileSync(new URL('../styles.css', import.meta.url), 'utf8')
  // 桌面規則：上下排
  const desktopIdx = css.indexOf('.send-btns {')
  assert.ok(desktopIdx > 0, '需有桌面 .send-btns 規則')
  const desktopBlock = css.slice(desktopIdx, css.indexOf('}', desktopIdx))
  assert.match(desktopBlock, /flex-direction:\s*column/)
  assert.match(desktopBlock, /align-self:\s*flex-end/)

  // 手機規則（≤640px）：左右並排、換行、字級與結構
  const mobileStart = css.indexOf('claude-api：手機維持左右並排')
  assert.ok(mobileStart > 0)
  const mobileIdx = css.indexOf('.send-btns {', mobileStart)
  assert.ok(mobileIdx > 0, '需有手機 .send-btns 規則')
  const mobileBlock = css.slice(mobileIdx, css.indexOf('}', mobileIdx))
  assert.match(mobileBlock, /flex-direction:\s*row/)

  const mobileBtnIdx = css.indexOf('.send-btns .send-btn {', mobileStart)
  assert.ok(mobileBtnIdx > 0)
  const mobileBtnBlock = css.slice(mobileBtnIdx, css.indexOf('}', mobileBtnIdx))
  assert.match(mobileBtnBlock, /white-space:\s*normal/)

  const mobileLabelIdx = css.indexOf('.send-btns .send-btn-label {', mobileStart)
  assert.ok(mobileLabelIdx > 0)
  const mobileLabelBlock = css.slice(mobileLabelIdx, css.indexOf('}', mobileLabelIdx))
  assert.match(mobileLabelBlock, /flex-direction:\s*column/)
})
