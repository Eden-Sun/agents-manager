import test from 'node:test'
import assert from 'node:assert/strict'
import { toPromptCache, toStatusInfo } from '../api/normalize'
import { cacheLabel, cacheView, coldWarning, contextFallback, contextTokens, tokensK, type CacheInput } from './composerCost'

const NOW = Date.parse('2026-10-04T12:00:00Z')
const nowS = Math.floor(NOW / 1000)
const ago = (m: number) => new Date(NOW - m * 60_000).toISOString()
const status = { context_used_pct: 45, context_used_tokens: 450_000, context_size: 1_000_000 }
const base: CacheInput = { kind: 'claude', status, lastApiAt: ago(10), ttlSecs: 3600, nowMs: NOW, working: false }

// claude 2.1.289 statusLine 實例形狀。
const pcJson = (over: Record<string, unknown> = {}) => ({
  context_window: { used_percentage: 45, context_window_size: 1_000_000, total_input_tokens: 450_000 },
  prompt_cache: {
    warm: true, expires_at: nowS + 25 * 60, ttl: '1h', hit_ratio: 0.994, recache_tokens_if_cold: 459_258, misses: 0,
    miss_causes: {}, last_miss_cause: null, cache_write_tokens: 434_165, requests: 213, caching_observed: true, ...over,
  },
})
const viaPc = (over: Record<string, unknown> = {}, extra: Partial<CacheInput> = {}) => ({ ...base, status: toStatusInfo(pcJson(over)), ...extra })
// daemon 的 run.prompt_cache（codex＝rollout 推算）。
const daemonPc = (over: Record<string, unknown> = {}) =>
  toPromptCache({
    source: 'rollout_estimate', warm: true, expires_at: nowS + 20 * 60, ttl_secs: 3600, recache_tokens_if_cold: 17_667, hit_ratio: 0.739,
    caching_observed: true, context_used_pct: 6.8, context_used_tokens: 17_667, context_size: 258_400, ...over,
  })
const codex = (over: Record<string, unknown> = {}, extra: Partial<CacheInput> = {}): CacheInput => ({
  kind: 'codex', status: null, promptCache: daemonPc(over), lastApiAt: null, ttlSecs: 3600, nowMs: NOW, working: false, ...extra,
})

test('tokensK / contextTokens', () => {
  assert.equal(tokensK(450_000), '450K')
  assert.equal(tokensK(1_250_000), '1.3M')
  assert.equal(tokensK(800), '800')
  assert.equal(contextTokens(status), 450_000)
  assert.equal(contextTokens({ context_used_pct: 50, context_used_tokens: null, context_size: 200_000 }), 100_000)
  assert.equal(contextTokens({ context_used_pct: 50, context_used_tokens: null, context_size: null }), null)
  assert.equal(contextTokens(null), null)
  // total_input_tokens 是累計值，百分比乘視窗優先。
  assert.equal(contextTokens({ context_used_pct: 10, context_used_tokens: 999_999, context_size: 200_000 }), 20_000)
})

test('toStatusInfo：解出 prompt_cache 精簡欄位（ttl "1h" → 3600）；沒有這塊為 undefined', () => {
  const s = toStatusInfo(pcJson())!
  assert.equal(s.prompt_cache?.warm, true)
  assert.equal(s.prompt_cache?.expires_at, nowS + 1500)
  assert.equal(s.prompt_cache?.ttl_secs, 3600)
  assert.equal(s.prompt_cache?.recache_tokens_if_cold, 459_258)
  assert.equal(toStatusInfo({ context_window: { used_percentage: 5 } })!.prompt_cache, undefined)
})

// ---- claude 有 prompt_cache：一律用它 ----
test('claude 熱：狀態列「熱（剩 25 分）」、輸入框不警示', () => {
  const v = cacheView(viaPc())!
  assert.equal(cacheLabel(v), '熱（剩 25 分）')
  assert.equal(coldWarning(v), null)
  // 不看 last_api_at／ttl。
  assert.equal(cacheLabel(cacheView(viaPc({}, { lastApiAt: ago(500), ttlSecs: null }))!), '熱（剩 25 分）')
  assert.equal(cacheLabel(cacheView(viaPc({ expires_at: nowS + 30 }))!), '熱（剩 1 分）')
})

test('claude 冷（warm=false）：狀態列「已冷」、輸入框警示寫 recache_tokens_if_cold', () => {
  const v = cacheView(viaPc({ warm: false, expires_at: nowS - 65 * 60 }))!
  assert.equal(cacheLabel(v), '已冷')
  assert.equal(coldWarning(v), '快取已過期，送出這則會重寫約 459K')
})

test('claude warm=true 但 expires_at 已過（statusLine 閒置不重送）以時間為準＝冷', () => {
  assert.equal(cacheView(viaPc({ warm: true, expires_at: nowS - 600 }))!.state, 'cold')
})

test('回合進行中一律熱、不警示', () => {
  const v = cacheView(viaPc({ warm: false, expires_at: nowS - 3600 }, { working: true }))!
  assert.equal(v.state, 'hot')
  assert.equal(cacheLabel(v), '熱')
  assert.equal(coldWarning(v), null)
  assert.equal(coldWarning(cacheView(codex({ warm: false }, { working: true }))), null)
})

test('缺 recache_tokens_if_cold：退用 context token 數；都沒有就寫整段 context', () => {
  assert.equal(coldWarning(cacheView(viaPc({ warm: false, recache_tokens_if_cold: null, expires_at: nowS - 3600 }))), '快取已過期，送出這則會重寫約 450K')
  const s = toStatusInfo({ prompt_cache: { warm: false, expires_at: 1, ttl: '1h' } })!
  assert.equal(coldWarning(cacheView({ ...base, status: s })), '快取已過期，送出這則會重寫整段 context')
})

test('沒有 prompt_cache（舊版 claude）才退回 last_api_at＋ttl 推算', () => {
  const s = toStatusInfo({ context_window: { used_percentage: 45, total_input_tokens: 450_000 } })!
  const cold = cacheView({ ...base, status: s, lastApiAt: ago(125) })!
  assert.equal(cold.state, 'cold')
  assert.equal(coldWarning(cold), '快取已過期，送出這則會重寫約 450K')
  assert.equal(cacheLabel(cacheView({ ...base, status: s, lastApiAt: ago(10) })!), '熱（剩 50 分）')
  // 推算也一樣：回合中不警示；沒有活動紀錄就沒有快取欄。
  assert.equal(coldWarning(cacheView({ ...base, status: s, lastApiAt: ago(500), working: true })), null)
  assert.equal(cacheView({ ...base, status: s, lastApiAt: null }), null)
})

// ---- codex：rollout 推估，標「約」；context 補進同一段 ----
test('codex 熱：「熱（約剩 20 分）」；冷：「已冷（約）」，警示寫近似的重寫量', () => {
  assert.equal(cacheLabel(cacheView(codex())!), '熱（約剩 20 分）')
  const cold = cacheView(codex({ warm: false, expires_at: nowS - 30 * 60 }))!
  assert.equal(cacheLabel(cold), '已冷（約）')
  assert.equal(coldWarning(cold), '快取已過期，送出這則會重寫約 18K')
})

test('codex 的 context 補進狀態列同一段格式；claude 已有 context 就不重複', () => {
  assert.deepEqual(contextFallback(codex()), { pct: '6.8%', detail: '18k/258k' })
  assert.equal(contextFallback({ ...codex(), status }), null)
  assert.equal(contextFallback(codex({ context_used_pct: null })), null)
})

test('codex 還沒讀到 rollout：沒有 prompt_cache 就退回推算；都沒有就不畫', () => {
  assert.equal(cacheView({ ...codex(), promptCache: null, lastApiAt: ago(125) })!.state, 'cold')
  assert.equal(cacheView({ ...codex(), promptCache: null }), null)
})

// ---- grok 完全不管 ----
test('grok（與未知 kind）：沒有快取欄、沒有警示、沒有 context 補充', () => {
  const g = { ...codex(), kind: 'grok' }
  assert.equal(cacheView(g), null)
  assert.equal(contextFallback(g), null)
  assert.equal(cacheView({ ...base, kind: undefined }), null)
})
