/**
 * 送出代價的兩個顯示（UI-DECISIONS「快取狀態：狀態列常駐、輸入框只在冷時警示」）：純函式，不碰 DOM。
 *
 * 只做 claude 與 codex（grok 一律不顯示）：
 * - 狀態列（context 那段後面）：`快取 熱（剩 23 分）`／`快取 已冷`；codex 是 rollout 推估，字面加「約」；
 *   codex 沒有 statusLine，context 也由 `prompt_cache` 補進同一段格式。
 * - 輸入框：平常不顯示；**只有快取已冷、而且不在回合中**才出一行警示：`快取已過期，送出這則會重寫約 811K`。
 *
 * 資料：daemon 的 `run.prompt_cache` 有就一律用它（claude＝statusLine，codex＝rollout 推算）——`warm`／`expires_at`
 * 判熱冷與剩餘，冷了寫 `recache_tokens_if_cold`。舊 daemon 的 claude 退用 statusLine 原 JSON 裡的 `prompt_cache`；
 * 都沒有（舊版 claude、codex 還沒讀到 rollout）才退回 `cacheState`（`last_api_at`＋`cache_ttl_secs`）推算。
 * 回合進行中快取一直被刷新：一律熱，不顯示過期。
 */
import { cacheState } from './cacheClock'
import type { PromptCacheInfo } from '../api/types'

export interface CostStatus {
  context_used_pct: number | null
  context_used_tokens: number | null
  context_size: number | null
  prompt_cache?: PromptCacheInfo | null
}

export interface CacheInput {
  kind: string | null | undefined
  /** daemon 的 `run.prompt_cache`。 */
  promptCache?: PromptCacheInfo | null
  status: CostStatus | null | undefined
  lastApiAt: string | null | undefined
  ttlSecs: number | null | undefined
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
}

/** 450000 → `450K`、1250000 → `1.3M`；500 以下直接寫數字。 */
export function tokensK(n: number): string {
  if (n >= 1_000_000) {
    const m = n / 1_000_000
    return `${m >= 10 || Number.isInteger(m) ? Math.round(m) : m.toFixed(1)}M`
  }
  if (n >= 1000) return `${Math.round(n / 1000)}K`
  return String(Math.round(n))
}

/** 狀態列用小寫 k（同 `811k/1M`）。 */
function tokensLower(n: number): string {
  return tokensK(n).replace('K', 'k')
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

/** statusLine 沒有 context（codex）時，由 `prompt_cache` 補：`{pct:'7%', detail:'17k/258k'}`。 */
export function contextFallback(input: CacheInput): { pct: string; detail: string | null } | null {
  if (input.kind !== 'claude' && input.kind !== 'codex') return null
  if (input.status?.context_used_pct != null) return null
  const pc = input.promptCache ?? input.status?.prompt_cache ?? null
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
  if (kind !== 'claude' && kind !== 'codex') return null
  const pc = input.promptCache ?? status?.prompt_cache ?? null
  const tokens = contextTokens(status, pc)
  const viaPc = pc ? fromPromptCache(pc, input, tokens) : undefined
  if (viaPc !== undefined) return viaPc
  const cs = cacheState(lastApiAt, ttlSecs, nowMs, working)
  if (!cs) return null
  if (working) return { state: 'hot', mins: null, working: true, approx: false, rewriteTokens: null }
  if (cs.level === 'cold') return { state: 'cold', mins: null, working: false, approx: false, rewriteTokens: tokens }
  return { state: 'hot', mins: Math.max(1, Math.ceil(cs.remainingSecs / 60)), working: false, approx: false, rewriteTokens: null }
}

/** 狀態列「快取」欄的值：`熱（剩 23 分）`／`熱`（回合中）／`已冷`；rollout 推估加「約」。 */
export function cacheLabel(v: CacheView): string {
  if (v.state === 'cold') return v.approx ? '已冷（約）' : '已冷'
  if (v.mins === null) return '熱'
  return `熱（${v.approx ? '約' : ''}剩 ${v.mins} 分）`
}

/** 輸入框警示：只有已冷且不在回合中才有；數字是冷了這次要重寫的 token（codex 為近似，同樣寫「約」）。 */
export function coldWarning(v: CacheView | null): string | null {
  if (!v || v.state !== 'cold' || v.working) return null
  return `快取已過期，送出這則會重寫${v.rewriteTokens !== null ? `約 ${tokensK(v.rewriteTokens)}` : '整段 context'}`
}
