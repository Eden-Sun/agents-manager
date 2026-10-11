/**
 * 環境設定關掉（或打開）Jev 之後，bot 上的 Jev 徽章要立刻跟著變（#1379）：
 * 儲存成功就重抓徽章那份帳本（force，不吃 60 秒節流）；儲存失敗不重抓。真的掛 `JudgePanel` 進 happy-dom。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, teardownDom, unmountAll, until } from '../testing/domHarness'
import { resetJudgeLedgerForTest, useJudgeLedger } from '../store/judgeLedger'
import { JudgePanel } from './JudgePanel'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetJudgeLedgerForTest()
  teardownDom()
})

type Route = { status: number; body: unknown }
/** 路由表：`METHOD /path`（不含 query）→ 回應；沒列到的回 404。 */
let routes: Record<string, Route> = {}
/** 每條路由被打了幾次。 */
let hits: Record<string, number> = {}

/** 每則測試開頭裝一次（`setupDom` 會換掉 fetch，所以不能在模組頂層裝）。 */
function routeFetch(next: Record<string, Route>) {
  hits = {}
  routes = next
  globalThis.fetch = (async (input: string, init?: { method?: string }) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    const key = `${init?.method ?? 'GET'} ${path.split('?')[0]}`
    hits[key] = (hits[key] ?? 0) + 1
    const r = routes[key] ?? { status: 404, body: {} }
    return new Response(JSON.stringify(r.body), { status: r.status })
  }) as unknown as typeof fetch
}

const SHADOW = 'GET /judge/shadow'
const SETTINGS = 'GET /judge/settings'
const PUT = 'PUT /judge/settings'

const settings = (enabled: boolean) => ({ enabled, projects: [], model: 'jev-1.13.0', key_present: true, key_error: null })
const shadow = (enabled: boolean) => ({ enabled, projects: [], model: 'jev-1.13.0', rows: [] })
const ok = (body: unknown): Route => ({ status: 200, body })

/** 徽章那份帳本剛抓過、還在 60 秒節流內，而且畫面上正顯示一顆徽章。 */
function seedLedgerVisible() {
  resetJudgeLedgerForTest()
  const at = new Date().toISOString()
  const hit = {
    id: 'hit',
    bot_id: 'bot-a',
    run_id: null,
    kind: null,
    matched_line: null,
    composer_idle: null,
    jev_is_live_ui: 0.9,
    jev_same_work: null,
    model: null,
    ms: null,
    input_tokens: null,
    error: null,
    cleared_at: null,
    assignment_id: null,
    claims_verified: null,
    asks_parent_action: null,
    regex_verdict: 'stuck_queued',
    at,
  }
  useJudgeLedger.setState({ visible: true, rows: [hit] as never, lastFetchAt: Date.now() })
}

const toggleEnabled = async (root: HTMLElement) => click(root.querySelector('.judge-switch input')!)
const submit = async (root: HTMLElement) =>
  act(async () => {
    root.querySelector('form.judge-panel')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
  })

test('關掉 Jev 儲存後，徽章那份帳本立刻重抓、visible 變 false（不吃節流）', async () => {
  routeFetch({ [SETTINGS]: ok(settings(true)), [PUT]: ok(settings(false)), [SHADOW]: ok(shadow(false)) })
  seedLedgerVisible()
  const root = await mount(<JudgePanel />)
  await until(() => root.querySelector('.judge-switch input') !== null, '設定讀進來')
  // 掛載時 JudgeLedger 自己會讀一次帳本；等它讀完，這裡只算儲存之後多打的那一次。
  await until(() => (hits[SHADOW] ?? 0) === 1, 'JudgeLedger 掛載時讀過一次帳本')
  const shadowBefore = hits[SHADOW] ?? 0

  await toggleEnabled(root)
  await submit(root)
  await until(() => !useJudgeLedger.getState().visible && (hits[SHADOW] ?? 0) === shadowBefore + 1, '儲存後帳本重抓', 2000)

  assert.equal(hits[PUT], 1)
  assert.equal(hits[SHADOW], shadowBefore + 1, '儲存成功只多抓一次帳本')
  assert.equal(useJudgeLedger.getState().visible, false, '徽章那份帳本的 visible 跟著變 false')
  assert.match(root.querySelector('.judge-note')?.textContent ?? '', /已儲存/)
})

test('儲存失敗（PUT 409 needs_key）不重抓帳本，徽章照舊', async () => {
  routeFetch({ [SETTINGS]: ok(settings(true)), [PUT]: { status: 409, body: { error: 'needs_key' } }, [SHADOW]: ok(shadow(false)) })
  seedLedgerVisible()
  const root = await mount(<JudgePanel />)
  await until(() => root.querySelector('.judge-switch input') !== null, '設定讀進來')
  await until(() => (hits[SHADOW] ?? 0) === 1, 'JudgeLedger 掛載時讀過一次帳本')
  const shadowBefore = hits[SHADOW] ?? 0

  await toggleEnabled(root)
  await submit(root)
  await until(() => root.querySelector('.judge-note.err') !== null, '顯示儲存失敗', 2000)

  assert.equal(hits[SHADOW] ?? 0, shadowBefore, '儲存失敗不打帳本')
  assert.equal(useJudgeLedger.getState().visible, true, '徽章不動')
})
