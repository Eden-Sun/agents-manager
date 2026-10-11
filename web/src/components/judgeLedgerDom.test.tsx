/**
 * 環境設定 → Jev 第二意見 的帳本區（issue #1327）：有資料、還沒觸發過、舊 daemon（404）、讀失敗可重試。
 * 真的掛 `JudgePanel` 進 happy-dom；fetch 換成本地替身，回的狀態碼與內容由每個測試決定。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import type { Bot } from '../api/types'
import { mount, setupDom, settle, teardownDom, unmountAll, click } from '../testing/domHarness'
import { resetStoreForTest, useStore } from '../store/store'
import { JudgePanel } from './JudgePanel'

afterEach(unmountAll)
before(setupDom)
after(() => {
  resetStoreForTest()
  teardownDom()
})

const SETTINGS = { enabled: false, projects: [], model: 'jev-1.13.0', key_present: false, key_error: null }
const ago = (h: number) => new Date(Date.now() - h * 3600_000).toISOString()

/** 本地 fetch 替身：`routes` 以不含 query 的路徑為鍵；`hits` 記下每個路徑被打了幾次。 */
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

const row = (p: Record<string, unknown>) => ({
  id: `r-${Math.random().toString(36).slice(2, 8)}`,
  bot_id: null,
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

function setBots() {
  resetStoreForTest()
  useStore.setState({ bots: [{ id: 'bot-alfa-0001', name: 'alfa' } as unknown as Bot] })
}

test('有資料：四個場景各一列計數、超過門檻照 SPEC 算、最近清單帶 bot 名', async () => {
  setBots()
  stubApi({
    '/judge/settings': { status: 200, body: SETTINGS },
    '/judge/shadow': {
      status: 200,
      body: {
        enabled: false,
        projects: [],
        model: 'jev-1.13.0',
        rows: [
          row({ regex_verdict: 'stuck_queued', at: ago(1), bot_id: 'bot-alfa-0001', jev_is_live_ui: 0.9, ms: 620 }),
          row({ regex_verdict: 'stuck_queued', at: ago(2), jev_is_live_ui: 0.3 }),
          row({ regex_verdict: 'report_evidence', at: ago(3), claims_verified: 0.6, error: 'boom' }),
        ],
      },
    },
  })
  const root = await mount(<JudgePanel />)
  await settle(200)
  const ledger = root.querySelector('.judge-ledger')!
  assert.ok(ledger, '帳本區要出現')
  const scenes = ledger.querySelectorAll('.judge-ledger-table')[0]!.querySelectorAll('tbody tr')
  assert.equal(scenes.length, 4, '四個場景各一列')
  const stuck = [...scenes].find((tr) => tr.textContent?.includes('卡在不認識的畫面'))!
  assert.match(stuck.textContent ?? '', /2 筆/, '卡住的場景：全部 2 筆')
  assert.match(stuck.textContent ?? '', /超過門檻 1/, '0.9 超過 0.7，0.3 不超過')
  const report = [...scenes].find((tr) => tr.textContent?.includes('完成回報的證據'))!
  assert.match(report.textContent ?? '', /錯誤 1/, '錯誤筆數要算到')
  const recent = ledger.querySelectorAll('.judge-ledger-table')[1]!.textContent ?? ''
  assert.match(recent, /alfa/, '找得到的 bot 顯示名字')
  assert.match(recent, /0\.90/, '機率欄照場景取欄位')
  assert.match(recent, /boom/, '錯誤欄')
})

test('還沒被觸發過：明講，並列出四個場景的觸發條件', async () => {
  setBots()
  stubApi({
    '/judge/settings': { status: 200, body: SETTINGS },
    '/judge/shadow': { status: 200, body: { enabled: false, projects: [], model: 'jev-1.13.0', rows: [] } },
  })
  const root = await mount(<JudgePanel />)
  await settle(200)
  const ledger = root.querySelector('.judge-ledger')!
  assert.match(ledger.textContent ?? '', /還沒被觸發過/)
  assert.equal(ledger.querySelector('.judge-ledger-table'), null, '沒有紀錄就不畫空表')
  const triggers = ledger.querySelectorAll('.judge-ledger-triggers li')
  assert.equal(triggers.length, 4)
  assert.match(ledger.textContent ?? '', /180 秒/, '卡住的觸發條件要寫出門檻時間')
})

test('舊 daemon（帳本 404）：設定照常，帳本區只說需要更新 daemon，不報錯', async () => {
  setBots()
  stubApi({ '/judge/settings': { status: 200, body: SETTINGS } })
  const root = await mount(<JudgePanel />)
  await settle(200)
  assert.ok(root.querySelector('input[type="password"]'), '設定表單還在')
  const ledger = root.querySelector('.judge-ledger')!
  assert.match(ledger.textContent ?? '', /需要更新 daemon/)
  assert.doesNotMatch(ledger.textContent ?? '', /讀不到帳本/)
})

test('讀失敗（500）：顯示真正的錯誤並給重試；按重試會再打一次', async () => {
  setBots()
  const hits = stubApi({
    '/judge/settings': { status: 200, body: SETTINGS },
    '/judge/shadow': { status: 500, body: { message: 'ledger unavailable' } },
  })
  const root = await mount(<JudgePanel />)
  await settle(200)
  const ledger = root.querySelector('.judge-ledger')!
  assert.match(ledger.textContent ?? '', /讀不到帳本/)
  assert.doesNotMatch(ledger.textContent ?? '', /需要更新 daemon/, '500 不是舊 daemon')
  assert.equal(hits['/judge/shadow'], 1)
  await click(ledger.querySelector('button')!)
  await settle(200)
  assert.equal(hits['/judge/shadow'], 2, '重試要重新打一次')
})
