import test from 'node:test'
import assert from 'node:assert/strict'
import { composerCostHint, contextTokens, idleText, tokensK } from './composerCost'

const NOW = Date.parse('2026-10-04T12:00:00Z')
const ago = (m: number) => new Date(NOW - m * 60_000).toISOString()
const status = { context_used_pct: 45, context_used_tokens: 450_000, context_size: 1_000_000 }
const base = { kind: 'claude', status, lastApiAt: ago(10), ttlSecs: 3600, nowMs: NOW, working: false }

test('tokensK / idleText 格式', () => {
  assert.equal(tokensK(450_000), '450K')
  assert.equal(tokensK(1_250_000), '1.3M')
  assert.equal(tokensK(800), '800')
  assert.equal(idleText(45 * 60), '45 分')
  assert.equal(idleText(125 * 60), '2 小時 5 分')
  assert.equal(idleText(120 * 60), '2 小時')
  assert.equal(idleText(27 * 3600), '1 天 3 小時')
})

test('contextTokens：優先用回報數字，沒有就用百分比乘視窗，都沒有為 null', () => {
  assert.equal(contextTokens(status), 450_000)
  assert.equal(contextTokens({ context_used_pct: 50, context_used_tokens: null, context_size: 200_000 }), 100_000)
  assert.equal(contextTokens({ context_used_pct: 50, context_used_tokens: null, context_size: null }), null)
  assert.equal(contextTokens(null), null)
})

test('context 段：有 token 數一起寫；浮點百分比收成一位；讀不到就沒有', () => {
  assert.equal(composerCostHint(base).context, 'context 45%（約 450K）')
  const f = composerCostHint({ ...base, status: { ...status, context_used_pct: 28.000000000000004, context_used_tokens: null, context_size: null } })
  assert.equal(f.context, 'context 28%')
  assert.equal(composerCostHint({ ...base, status: null }).context, null)
  assert.equal(composerCostHint({ ...base, status: { context_used_pct: null, context_used_tokens: null, context_size: null } }).context, null)
})

test('快取還熱：寫剩幾分（無條件進位、至少 1）', () => {
  const h = composerCostHint(base)
  assert.equal(h.cache?.kind, 'hot')
  assert.equal(h.cache?.text, '快取還熱（剩 50 分）')
  assert.equal(composerCostHint({ ...base, lastApiAt: new Date(NOW - 3599_000).toISOString() }).cache?.text, '快取還熱（剩 1 分）')
})

test('快取已過期：警示文案含閒置時間與重讀 token 數', () => {
  const h = composerCostHint({ ...base, lastApiAt: ago(125) })
  assert.equal(h.cache?.kind, 'cold')
  assert.equal(h.cache?.text, '快取已過期（閒置 2 小時 5 分），送出這則會重新讀入整段 context（約 450K）')
  assert.equal(h.cache?.short, '快取已過期，送出重讀 450K')
  // 不知道 token 數：文案不帶括號數字。
  const n = composerCostHint({ ...base, lastApiAt: ago(125), status: { ...status, context_used_pct: null, context_used_tokens: null } })
  assert.equal(n.cache?.text, '快取已過期（閒置 2 小時 5 分），送出這則會重新讀入整段 context')
  assert.equal(n.cache?.short, '快取已過期，送出重讀')
})

test('回合進行中不顯示過期；grok（TTL 不明）與沒有活動紀錄不顯示快取段', () => {
  const w = composerCostHint({ ...base, lastApiAt: ago(500), working: true })
  assert.equal(w.cache?.kind, 'hot')
  assert.equal(composerCostHint({ ...base, ttlSecs: null }).cache, null)
  assert.equal(composerCostHint({ ...base, lastApiAt: null }).cache, null)
  assert.equal(composerCostHint({ ...base, ttlSecs: null }).context, 'context 45%（約 450K）')
})

// ---- claude ≥ 2.1.289 的 statusLine `prompt_cache`：有就一律用它 ----
import { toPromptCache, toStatusInfo } from '../api/normalize'

const pcJson = (over: Record<string, unknown> = {}) => ({
  context_window: { used_percentage: 45, context_window_size: 1_000_000, total_input_tokens: 450_000 },
  prompt_cache: {
    warm: true, expires_at: Math.floor(NOW / 1000) + 25 * 60, ttl: '1h', hit_ratio: 0.994, recache_tokens_if_cold: 459_258, misses: 0,
    miss_causes: {}, last_miss_cause: null, cache_write_tokens: 434_165, requests: 213, caching_observed: true, ...over,
  },
})
const viaPc = (over: Record<string, unknown> = {}, extra: Partial<typeof base> = {}) =>
  composerCostHint({ ...base, status: toStatusInfo(pcJson(over)), ...extra })

test('toStatusInfo：解出 prompt_cache 精簡欄位（ttl "1h" → 3600）；沒有這塊為 undefined', () => {
  const s = toStatusInfo(pcJson())!
  assert.equal(s.prompt_cache?.warm, true)
  assert.equal(s.prompt_cache?.expires_at, Math.floor(NOW / 1000) + 1500)
  assert.equal(s.prompt_cache?.ttl_secs, 3600)
  assert.equal(s.prompt_cache?.recache_tokens_if_cold, 459_258)
  assert.equal(s.prompt_cache?.source, null)
  assert.equal(s.context_used_pct, 45)
  assert.equal(toStatusInfo({ context_window: { used_percentage: 5 } })!.prompt_cache, undefined)
})

test('prompt_cache：warm＋expires_at 決定剩餘，不看 last_api_at／ttl（TTL 不明也照畫）', () => {
  const h = viaPc({}, { lastApiAt: ago(500), ttlSecs: undefined })
  assert.equal(h.cache?.kind, 'hot')
  assert.equal(h.cache?.text, '快取還熱（剩 25 分）')
})

test('prompt_cache：warm=false 為冷，寫 recache_tokens_if_cold；閒置由 expires_at−ttl 算', () => {
  const h = viaPc({ warm: false, expires_at: Math.floor(NOW / 1000) - 65 * 60, ttl: '1h' })
  assert.equal(h.cache?.kind, 'cold')
  assert.equal(h.cache?.text, '快取已過期（閒置 2 小時 5 分），送出這則會重寫約 459K')
  assert.equal(h.cache?.short, '快取已過期，送出重寫 459K')
  assert.equal(h.context, 'context 45%（約 450K）')
})

test('prompt_cache：warm=true 但 expires_at 已過（statusLine 閒置不重送）以時間為準＝冷', () => {
  const h = viaPc({ warm: true, expires_at: Math.floor(NOW / 1000) - 10 * 60 })
  assert.equal(h.cache?.kind, 'cold')
})

test('prompt_cache：回合進行中不顯示過期', () => {
  const h = viaPc({ warm: false, expires_at: Math.floor(NOW / 1000) - 3600 }, { working: true })
  assert.equal(h.cache?.kind, 'hot')
})

test('prompt_cache 缺 recache_tokens_if_cold：退用 context token 數；都沒有就不寫數字', () => {
  const exp = Math.floor(NOW / 1000) - 3600
  assert.match(viaPc({ warm: false, recache_tokens_if_cold: null, expires_at: exp }).cache!.text, /^快取已過期（閒置 2 小時），送出這則會重寫約 450K$/)
  const s = toStatusInfo({ prompt_cache: { warm: false, expires_at: 1, ttl: '1h' } })!
  assert.match(composerCostHint({ ...base, status: s }).cache!.text, /送出這則會重寫整段 context$/)
})

test('沒有 prompt_cache（舊版 claude／codex）才退回 last_api_at＋ttl 推算', () => {
  const s = toStatusInfo({ context_window: { used_percentage: 45, total_input_tokens: 450_000 } })!
  const h = composerCostHint({ ...base, status: s, lastApiAt: ago(125) })
  assert.equal(h.cache?.text, '快取已過期（閒置 2 小時 5 分），送出這則會重新讀入整段 context（約 450K）')
})

// ---- daemon 的 run.prompt_cache（claude＝statusline、codex＝rollout 推算）與 grok ----
const daemonPc = (over: Record<string, unknown> = {}) =>
  toPromptCache({
    source: 'rollout_estimate', warm: true, expires_at: Math.floor(NOW / 1000) + 20 * 60, ttl_secs: 3600, recache_tokens_if_cold: 17_667, hit_ratio: 0.739,
    caching_observed: true, context_used_pct: 6.8, context_used_tokens: 17_667, context_size: 258_400, ...over,
  })

test('codex（run.prompt_cache、沒有 statusLine）：context 與快取都標「約」', () => {
  const h = composerCostHint({ kind: 'codex', status: null, promptCache: daemonPc(), lastApiAt: null, ttlSecs: 3600, nowMs: NOW, working: false })
  assert.equal(h.context, 'context 6.8%（約 18K）')
  assert.equal(h.cache?.text, '快取還熱（約剩 20 分）')
  assert.equal(h.cache?.short, '快取熱 約剩 20 分')
})

test('codex 冷：約閒置、重寫約 recache_tokens_if_cold', () => {
  const h = composerCostHint({
    kind: 'codex', status: null, lastApiAt: null, ttlSecs: 3600, nowMs: NOW, working: false,
    promptCache: daemonPc({ warm: false, expires_at: Math.floor(NOW / 1000) - 30 * 60 }),
  })
  assert.equal(h.cache?.kind, 'cold')
  assert.equal(h.cache?.text, '快取已過期（約閒置 1 小時 30 分），送出這則會重寫約 18K')
})

test('claude 的 run.prompt_cache 優先於 statusLine 原 JSON，context 用 daemon 算好的 token 數', () => {
  const pc = daemonPc({ source: 'statusline', context_used_pct: 45, context_used_tokens: 450_000, context_size: 1_000_000, expires_at: Math.floor(NOW / 1000) + 5 * 60 })
  const stale = toStatusInfo(pcJson({ warm: false, expires_at: 1 }))
  const h = composerCostHint({ ...base, status: stale, promptCache: pc })
  assert.equal(h.cache?.text, '快取還熱（剩 5 分）')
  assert.equal(h.context, 'context 45%（約 450K）')
})

test('grok（與未知 kind）完全不顯示，即使有 last_api_at／ttl／status', () => {
  const h = composerCostHint({ ...base, kind: 'grok', promptCache: daemonPc() })
  assert.deepEqual(h, { context: null, cache: null })
  assert.deepEqual(composerCostHint({ ...base, kind: undefined }), { context: null, cache: null })
})

test('contextTokens：total_input_tokens 是累計值，百分比乘視窗優先', () => {
  assert.equal(contextTokens({ context_used_pct: 10, context_used_tokens: 999_999, context_size: 200_000 }), 20_000)
})
