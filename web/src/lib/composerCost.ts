/**
 * 快取代價提示：Claude 依讀數／TTL 顯示熱冷；Codex 固定提示由 StatusCache 顯示，
 * 不再用 rollout 的舊 TTL 推算過期或重寫警示。context 仍可使用 rollout token 讀數。
 */
import { cacheLevel, cacheState } from './cacheClock'
import { solWindowApplies } from './codexContextWindow'
import type { PromptCacheInfo } from '../api/types'
import { t } from './i18n'

export interface CostStatus {
  context_used_pct: number | null
  context_used_tokens: number | null
  context_size: number | null
  prompt_cache?: PromptCacheInfo | null
  /** 剛壓縮／resume、還沒有新讀數（daemon 的 `_agm_context_unknown`）。 */
  context_unknown?: boolean
}

export interface CacheInput {
  kind: string | null | undefined
  /** daemon 的 `run.prompt_cache`。 */
  promptCache?: PromptCacheInfo | null
  /** bot 的有效模型（`runtime_model ?? model`）；只有 gpt-6.1-sol 會改 context 分母（見 `codexContextWindow`）。 */
  model?: string | null
  status: CostStatus | null | undefined
  lastApiAt: string | null | undefined
  /** 主力 cache 保溫讓 cache 變熱的時間（`run.cache_kept_warm_at`）；只影響退回 `last_api_at` 推算的那條路。 */
  keptWarmAt?: string | null
  ttlSecs: number | null | undefined
  /** claude-api：最近一次呼叫讀到的整段 context token 數（冷掉時估重寫量）；沒有就 null。 */
  apiLastTokens?: number | null
  nowMs: number
  working: boolean
}

export interface CacheView {
  state: 'hot' | 'cold'
  /** 熱：剩幾分（無條件進位、至少 1）；回合進行中或不知道剩多久為 null。 */
  mins: number | null
  working: boolean
  /** rollout 推算（字面加「約」）。 */
  approx: boolean
  /** 冷了送出要重寫幾 token；不知道為 null。 */
  rewriteTokens: number | null
  /** 剛壓縮／resume，context 大小要等下一回合才知道（警示不寫「整段」嚇人）。 */
  contextUnknown?: boolean
}

/** 450000 → `450K`、1250000 → `1.3M`；500 以下直接寫數字。單位看四捨五入之後的值：999500 不寫成 1000K。 */
export function tokensK(n: number): string {
  if (n >= 999_500) {
    // 0.1M 精度先取整再判斷：999500～1049999 得 1M，9950000 起寫 10M（不寫 10.0M）。
    const m = Math.round(n / 100_000) / 10
    return `${m >= 10 || Number.isInteger(m) ? Math.round(m) : m.toFixed(1)}M`
  }
  if (n >= 1000) return `${Math.round(n / 1000)}K`
  return String(Math.round(n))
}

/** 狀態列與狀態卡用小寫 k（同 `811k/1M`）。 */
export function tokensLower(n: number): string {
  return tokensK(n).replace('K', 'k')
}

/** 168800 → `169k`, 1000000 → `1M`（狀態列的 context token 數）。 */
export function compactTokens(n: number): string {
  // 進位後滿 1000k 就換成 M（999600 寫 1M，不寫 1000k）；M 的一位小數進位到 10 時寫整數。
  if (n >= 999_500) {
    const m = Math.round(n / 100_000) / 10
    return `${m >= 10 || Number.isInteger(m) ? Math.round(m) : m.toFixed(1)}M`
  }
  if (n >= 1000) return `${Math.round(n / 1000)}k`
  return String(n)
}

/** claude 給的百分比是原始浮點（28.000000000000004）：最多一位小數。 */
function pctText(n: number): string {
  const r = Math.round(n * 10) / 10
  return `${Number.isInteger(r) ? r : r.toFixed(1)}%`
}

/**
 * 目前 context 約多少 token：daemon 給的（`prompt_cache.context_used_tokens`）→ 百分比乘視窗大小 →
 * statusLine 的 `total_input_tokens`（累計值，可能比 context 大，最後才用）。
 */
export function contextTokens(s: CostStatus | null | undefined, pc?: PromptCacheInfo | null): number | null {
  if (pc?.context_used_tokens != null && pc.context_used_tokens > 0) return pc.context_used_tokens
  const pct = pc?.context_used_pct ?? s?.context_used_pct ?? null
  const size = pc?.context_size ?? s?.context_size ?? null
  if (pct !== null && size !== null && size > 0) return (pct / 100) * size
  if (s && s.context_used_tokens !== null && s.context_used_tokens > 0) return s.context_used_tokens
  return null
}

/**
 * statusLine 沒有 context（codex）時，由 `prompt_cache` 補：`{pct:'7%', detail:'17k/258k'}`。
 * 有 `status.context_used_pct` 就回 null：那一段由 `StatusLineBar` 畫（主顯示路徑，gpt-6.1-sol 在那裡也改用 500k），這裡不重複。
 */
export function contextFallback(input: CacheInput): { pct: string; detail: string | null } | null {
  if (input.kind !== 'claude' && input.kind !== 'codex') return null
  if (input.status?.context_used_pct != null) return null
  const pc = input.promptCache ?? input.status?.prompt_cache ?? null
  // gpt-6.1-sol 的 context 整段由 `StatusLineBar` 畫（實際 token ÷ 500k，不論有沒有 statusLine）；這裡不重複，避免同一段畫兩次。
  if (solWindowApplies(input.kind, input.model)) return null
  if (pc?.context_used_pct == null) return null
  const used = contextTokens(input.status, pc)
  const detail = used !== null && pc.context_size ? `${tokensLower(used)}/${tokensLower(pc.context_size)}` : null
  return { pct: pctText(pc.context_used_pct), detail }
}

/** `prompt_cache`：warm／expires_at 說了算；看起來沒資料回 undefined（改走推算）。 */
function fromPromptCache(pc: PromptCacheInfo, input: CacheInput, fallbackTokens: number | null): CacheView | undefined {
  const { nowMs, working } = input
  if (pc.warm === null && pc.expires_at === null) return undefined
  const approx = pc.source === 'rollout_estimate'
  if (working) return { state: 'hot', mins: null, working: true, approx, rewriteTokens: null }
  // expires_at 是 epoch 秒（保險：> 1e12 當毫秒）。
  const exp = pc.expires_at !== null ? (pc.expires_at > 1e12 ? pc.expires_at : pc.expires_at * 1000) : null
  const remaining = exp !== null ? Math.round((exp - nowMs) / 1000) : null
  // statusLine／rollout 閒置時不會更新，所以 `warm:true` 也可能已經過了 expires_at：以時間為準。
  if (pc.warm !== false && (remaining === null || remaining > 0)) {
    return { state: 'hot', mins: remaining === null ? null : Math.max(1, Math.ceil(remaining / 60)), working: false, approx, rewriteTokens: null }
  }
  return { state: 'cold', mins: null, working: false, approx, rewriteTokens: pc.recache_tokens_if_cold ?? fallbackTokens }
}

/** 這顆 bot 現在的快取狀態；grok、沒有任何資料回 null。 */
export function cacheView(input: CacheInput): CacheView | null {
  const { kind, status, lastApiAt, ttlSecs, nowMs, working } = input
  if (kind !== 'claude' && kind !== 'claude-api') return null
  const pc = input.promptCache ?? status?.prompt_cache ?? null
  // claude-api 沒有 statusLine：重寫量用最近一次呼叫的整段 context。
  const tokens = kind === 'claude-api' ? (input.apiLastTokens ?? null) : contextTokens(status, pc)
  const viaPc = pc ? fromPromptCache(pc, input, tokens) : undefined
  if (viaPc !== undefined) return markUnknown(viaPc, status)
  const cs = cacheState(lastApiAt, ttlSecs, nowMs, working, input.keptWarmAt)
  if (!cs) return null
  if (working) return { state: 'hot', mins: null, working: true, approx: false, rewriteTokens: null }
  if (cs.level === 'cold') return markUnknown({ state: 'cold', mins: null, working: false, approx: false, rewriteTokens: tokens }, status)
  return { state: 'hot', mins: Math.max(1, Math.ceil(cs.remainingSecs / 60)), working: false, approx: false, rewriteTokens: null }
}

function markUnknown(v: CacheView, status: CostStatus | null | undefined): CacheView {
  return v.state === 'cold' && v.rewriteTokens === null && status?.context_unknown ? { ...v, contextUnknown: true } : v
}

/** 狀態列「快取」欄的值：`熱（剩 23 分）`／`熱`（回合中）／`已冷`；rollout 推估加「約」。 */
export function cacheLabel(v: CacheView): string {
  if (v.state === 'cold') return v.approx ? t('已冷（約）') : t('已冷')
  if (v.mins === null) return t('熱')
  return v.approx
    ? t('熱（約剩 {n} 分）', { n: v.mins })
    : t('熱（剩 {n} 分）', { n: v.mins })
}

/** 輸入框警示：只有已冷且不在回合中才有；數字是冷了這次要重寫的 token（codex 為近似，同樣寫「約」）。 */
export function coldWarning(v: CacheView | null): string | null {
  if (!v || v.state !== 'cold' || v.working) return null
  return v.rewriteTokens !== null
    ? t('快取已過期，送出這則會重寫約 {tokens}', { tokens: tokensK(v.rewriteTokens) })
    : v.contextUnknown
      ? t('快取已過期；剛壓縮或接回過，context 大小下一回合才知道')
      : t('快取已過期，送出這則會重寫整段 context')
}

/** claude-api 各模型的快取寫入單價（美元／百萬 token；5 分鐘與 1 小時）。官方價目表：5 分鐘 1.25×、1 小時 2× 基本輸入價。 */
const API_WRITE_USD_PER_M: Record<string, { m5: number; h1: number }> = {
  'claude-opus-5-5': { m5: 5, h1: 8 },
  'claude-fable-5-2': { m5: 10, h1: 16 },
}

/** 冷掉後重寫 `tokens` 個 token 約多少美元（目前 TTL 的 write 單價，fast 2×）；不認得的模型 null。 */
export function apiRewriteUsd(tokens: number, model: string | null | undefined, ttlSecs: number | null | undefined, fast: boolean): number | null {
  const rate = model ? API_WRITE_USD_PER_M[model] : undefined
  return rate ? (tokens / 1_000_000) * (ttlSecs === 3600 ? rate.h1 : rate.m5) * (fast ? 2 : 1) : null
}

/** `3:42`／`1:02:05`：快取倒數的剩餘時間。 */
export function clockText(secs: number): string {
  const s = Math.max(0, Math.floor(secs))
  const p = (n: number) => String(n).padStart(2, '0')
  const h = Math.floor(s / 3600)
  return h > 0 ? `${h}:${p(Math.floor((s % 3600) / 60))}:${p(s % 60)}` : `${Math.floor(s / 60)}:${p(s % 60)}`
}

export interface ApiCacheInfo { last_request_at: string | null; ttl_secs: number | null; expires_at: string | null; working: boolean }

/**
 * claude-api 狀態列的快取倒數（daemon 的 `api-usage.cache` 給精確時間，這裡只減「現在」）：
 * 回合進行中＝`熱`；熱＝`剩 3:42`（顏色門檻同 `cacheLevel`）；冷＝`已冷`，認得模型時附重寫成本（目前 context token × 寫入單價）。
 * 狀態判定走 `cacheView`，跟 claude CLI bot 的快取欄同一套。沒有任何已完成請求回 null。
 */
export function apiCacheChip(cache: ApiCacheInfo, nowMs: number, ctxTokens: number | null, model: string | null | undefined, fast: boolean): { label: string; level: 'fresh' | 'warn' | 'low' | 'cold' | 'hot'; title: string } | null {
  if (cache.working) return { label: t('熱'), level: 'hot', title: t('回合進行中，快取是熱的{when}', { when: '' }) }
  if (!cache.last_request_at || !cache.ttl_secs || !cache.expires_at) return null
  const view = cacheView({ kind: 'claude-api', status: null, lastApiAt: cache.last_request_at, ttlSecs: cache.ttl_secs, nowMs, working: false, apiLastTokens: ctxTokens })
  const remaining = Math.ceil((Date.parse(cache.expires_at) - nowMs) / 1000)
  if (!view || view.state === 'cold' || !(remaining > 0)) {
    const usd = ctxTokens !== null ? apiRewriteUsd(ctxTokens, model, cache.ttl_secs, fast) : null
    return {
      label: usd === null ? t('已冷') : t('已冷（重寫約 ${cost}）', { cost: usd.toFixed(2) }),
      level: 'cold',
      title: apiColdWarning(view ?? { state: 'cold', mins: null, working: false, approx: false, rewriteTokens: ctxTokens }, model, cache.ttl_secs, fast, ctxTokens) ?? t('快取已冷'),
    }
  }
  return { label: t('剩 {time}', { time: clockText(remaining) }), level: cacheLevel(remaining), title: t('快取 {ttl} 保留，倒數至最近一次請求完成後 {ttl}', { ttl: cache.ttl_secs === 3600 ? '1h' : '5m' }) }
}

/**
 * claude-api 的快取冷提示（只提示、不擋送出）：已冷，或剩不到 1 分鐘時，講下一則約重寫多少 token、約多少錢。
 * 單價用這顆 bot 目前的 TTL 對應的 write 單價（fast 2×）；不認得的模型只講 token 數。
 */
export function apiColdWarning(v: CacheView | null, model: string | null | undefined, ttlSecs: number | null | undefined, fast: boolean, lastTokens?: number | null): string | null {
  if (!v || v.working) return null
  const cold = v.state === 'cold'
  if (!cold && v.mins !== 1) return null
  const rate = model ? API_WRITE_USD_PER_M[model] : undefined
  // 已冷的 view 自帶重寫量；還熱（剩不到 1 分鐘）的 view 沒有，退回最近一次呼叫的整段 context。
  const tokens = v.rewriteTokens ?? lastTokens ?? null
  const lead = cold ? t('快取已冷') : t('快取剩不到 1 分鐘')
  if (tokens === null) return t('{lead}：下一則會重寫整段 context', { lead })
  if (!rate) return t('{lead}：下一則約重寫 {tokens} token', { lead, tokens: tokensK(tokens) })
  const usd = apiRewriteUsd(tokens, model, ttlSecs, fast)!
  return t('{lead}：下一則約重寫 {tokens} token（約 ${cost}）', { lead, tokens: tokensK(tokens), cost: usd.toFixed(2) })
}
