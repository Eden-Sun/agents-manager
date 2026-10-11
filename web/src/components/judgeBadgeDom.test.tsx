/**
 * bot 上的 Jev 徽章（issue #1328）：真的掛 `JudgeBadge` 進 happy-dom。
 * 超過門檻才出現；點開看說明；關掉之後不再出現；Jev 沒開／帳本 404 完全不顯示；帳本重抓最短 60 秒一次。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { act, click, mount, setupDom, settle, teardownDom, unmountAll } from '../testing/domHarness'
import { resetJudgeLedgerForTest, useJudgeLedger } from '../store/judgeLedger'
import { DISMISSED_KEY } from '../lib/judgeBadge'
import { JudgeBadge } from './JudgeBadge'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetJudgeLedgerForTest()
  teardownDom()
})

const ago = (h: number) => new Date(Date.now() - h * 3600_000).toISOString()
const row = (p: Record<string, unknown>) => ({
  bot_id: 'bot-a',
  run_id: null,
  kind: null,
  matched_line: null,
  composer_idle: null,
  jev_is_live_ui: null,
  jev_same_work: null,
  model: null,
  ms: null,
  input_tokens: null,
  error: null,
  cleared_at: null,
  assignment_id: null,
  claims_verified: null,
  asks_parent_action: null,
  ...p,
})

/** 本地 fetch 替身：路徑（不含 query）→ 狀態碼與內容；`hits` 記下每個路徑被打了幾次。 */
function stubApi(routes: Record<string, { status: number; body?: unknown }>) {
  const hits: Record<string, number> = {}
  globalThis.fetch = (async (input: string) => {
    const path = String(input).replace(/^\/api/, '')
    if (path === '/session') return new Response(JSON.stringify({ token: 'test-token' }), { status: 200 })
    const key = path.split('?')[0]!
    hits[key] = (hits[key] ?? 0) + 1
    const r = routes[key] ?? { status: 404, body: {} }
    return new Response(JSON.stringify(r.body ?? {}), { status: r.status })
  }) as unknown as typeof fetch
  return hits
}

function shadowWith(rows: unknown[], enabled = true) {
  return { status: 200, body: { enabled, projects: [], model: 'jev-1.13.0', rows } }
}

function seed(rows: unknown[], visible = true) {
  resetJudgeLedgerForTest()
  // 真實的 app 只有成功重抓後才會 visible，那時 lastFetchAt 一定有值（「近 24 小時」以它為準）。
  useJudgeLedger.setState({ rows: rows as never, visible, dismissed: new Set(), lastFetchAt: Date.now() })
}

test('超過門檻才出現徽章；title 寫出場景與機率', async () => {
  seed([row({ id: 'hit', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  const btn = root.querySelector<HTMLButtonElement>('.judge-badge')
  assert.ok(btn, '超過門檻要有徽章')
  assert.match(btn.title, /卡在不認識的畫面/)
  assert.match(btn.title, /0\.90/)
})

test('低於門檻、別的 bot、超過 24 小時：都不出現', async () => {
  seed([
    row({ id: 'low', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.2 }),
    row({ id: 'other', regex_verdict: 'stuck_queued', at: ago(1), bot_id: 'bot-b', jev_is_live_ui: 0.99 }),
    row({ id: 'old', regex_verdict: 'stuck_queued', at: ago(48), jev_is_live_ui: 0.99 }),
  ])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.equal(root.querySelector('.judge-badge'), null)
})

test('點開：寫明場景、機率、遮罩後的命中行，以及「不會自動動作」', async () => {
  seed([row({ id: 'hit', regex_verdict: 'same_work', at: ago(1), jev_same_work: 0.6, matched_line: 'task A ↔ task B' })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  await click(root.querySelector('.judge-badge')!)
  await settle(20)
  const pop = root.querySelector('.judge-badge-pop')!
  assert.ok(pop, '點了要開浮層')
  assert.match(pop.textContent ?? '', /撞題提示/)
  assert.match(pop.textContent ?? '', /0\.60/)
  assert.match(pop.textContent ?? '', /task A ↔ task B/)
  assert.match(pop.textContent ?? '', /不會自動動作/)
})

test('關掉這則：立刻不見，並記在 localStorage（重開也不再出現）', async () => {
  seed([row({ id: 'gone-1', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })])
  const root = await mount(<JudgeBadge botId="bot-a" />)
  await click(root.querySelector('.judge-badge')!)
  await settle(20)
  const dismiss = [...root.querySelectorAll('button')].find((b) => b.textContent?.includes('關掉這則'))!
  await click(dismiss)
  await settle(20)
  assert.equal(root.querySelector('.judge-badge'), null, '關掉之後不顯示')
  assert.ok(useJudgeLedger.getState().dismissed.has('gone-1'))
  assert.match(localStorage.getItem(DISMISSED_KEY) ?? '', /gone-1/)
})

test('Jev 沒開：帳本回 enabled=false，徽章完全不顯示', async () => {
  stubApi({ '/judge/shadow': shadowWith([row({ id: 'x', regex_verdict: 'stuck_queued', at: ago(1), jev_is_live_ui: 0.9 })], false) })
  resetJudgeLedgerForTest()
  await useJudgeLedger.getState().refresh({ force: true })
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.equal(root.querySelector('.judge-badge'), null)
})

test('帳本 404（舊 daemon）：不顯示、不報錯', async () => {
  stubApi({})
  resetJudgeLedgerForTest()
  await useJudgeLedger.getState().refresh({ force: true })
  const root = await mount(<JudgeBadge botId="bot-a" />)
  assert.equal(root.querySelector('.judge-badge'), null)
  assert.equal(root.textContent?.trim(), '', '不畫任何文字')
})

test('帳本重抓最短 60 秒一次：連續要求只打一次；強制重抓才再打', async () => {
  const hits = stubApi({ '/judge/shadow': shadowWith([]) })
  resetJudgeLedgerForTest()
  await act(async () => {
    await useJudgeLedger.getState().refresh()
    await useJudgeLedger.getState().refresh()
  })
  assert.equal(hits['/judge/shadow'], 1, '60 秒內第二次不打')
  await act(async () => {
    await useJudgeLedger.getState().refresh({ force: true })
  })
  assert.equal(hits['/judge/shadow'], 2, '強制重抓要打')
})
