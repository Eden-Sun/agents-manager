/**
 * 輸入框的「送出代價」提示（UI-DECISIONS「輸入框上方的 context／快取提示」）：純函式，不碰 DOM。
 *
 * 只做 claude 與 codex（grok 一律不顯示）；兩段，都只提示、不擋送出：
 * - context 用量：`context_used_pct`（有 token 數就一起寫）；讀不到就沒有這段。
 * - 快取：daemon 的 `run.prompt_cache` 有就一律用它（claude＝statusLine，codex＝rollout 推算、標「約」）——
 *   `warm`／`expires_at` 判熱冷與剩餘，冷了寫 `recache_tokens_if_cold`。舊 daemon 的 claude 退用 statusLine 原 JSON 裡的
 *   `prompt_cache`；都沒有（舊版 claude、codex 還沒讀到 rollout）才退回 `cacheState`（`last_api_at`＋`cache_ttl_secs`）推算。
 *   回合進行中快取一直被刷新，不顯示過期。
 */
import { cacheState } from './cacheClock'
import type { PromptCacheInfo } from '../api/types'

export interface CostStatus {
  context_used_pct: number | null
  context_used_tokens: number | null
  context_size: number | null
  prompt_cache?: PromptCacheInfo | null
}

export interface ComposerCostInput {
  kind: string | null | undefined
  /** daemon 的 `run.prompt_cache`。 */
  promptCache?: PromptCacheInfo | null
  status: CostStatus | null | undefined
  lastApiAt: string | null | undefined
  ttlSecs: number | null | undefined
  nowMs: number
  working: boolean
}

export interface ComposerCostHint {
  /** `context 45%（約 450K）`；讀不到用量為 null。 */
  context: string | null
  cache: {
    kind: 'hot' | 'cold'
    /** 桌機完整句。 */
    text: string
    /** 手機縮短版（一行放得下）。 */
    short: string
  } | null
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

/** 閒置時間：`45 分`、`2 小時 5 分`、`1 天 3 小時`；整數單位不寫「0 分」。 */
export function idleText(secs: number): string {
  const mins = Math.max(0, Math.floor(secs / 60))
  if (mins < 60) return `${mins} 分`
  const hours = Math.floor(mins / 60)
  const m = mins % 60
  if (hours < 24) return m ? `${hours} 小時 ${m} 分` : `${hours} 小時`
  const d = Math.floor(hours / 24)
  const h = hours % 24
  return h ? `${d} 天 ${h} 小時` : `${d} 天`
}

/** `prompt_cache`：warm／expires_at 說了算；看起來沒資料回 undefined（改走推算）。`approx`＝rollout 推算，字面加「約」。 */
function fromPromptCache(pc: PromptCacheInfo, input: ComposerCostInput, fallbackTokens: number | null): ComposerCostHint['cache'] | undefined {
  const { nowMs, working } = input
  if (pc.warm === null && pc.expires_at === null) return undefined
  if (working) return { kind: 'hot', text: '快取還熱（回合進行中）', short: '快取還熱' }
  const approx = pc.source === 'rollout_estimate' ? '約' : ''
  const ttl = pc.ttl_secs ?? input.ttlSecs ?? 3600
  // expires_at 是 epoch 秒（保險：> 1e12 當毫秒）。
  const exp = pc.expires_at !== null ? (pc.expires_at > 1e12 ? pc.expires_at : pc.expires_at * 1000) : null
  const remaining = exp !== null ? Math.round((exp - nowMs) / 1000) : null
  // statusLine／rollout 閒置時不會更新，所以 `warm:true` 也可能已經過了 expires_at：以時間為準。
  if (pc.warm !== false && (remaining === null || remaining > 0)) {
    const mins = remaining === null ? null : Math.max(1, Math.ceil(remaining / 60))
    return mins === null
      ? { kind: 'hot', text: '快取還熱', short: '快取還熱' }
      : { kind: 'hot', text: `快取還熱（${approx}剩 ${mins} 分）`, short: `快取熱 ${approx}剩 ${mins} 分` }
  }
  const lastAt = exp !== null ? exp - ttl * 1000 : Date.parse(input.lastApiAt ?? '')
  const idle = Number.isFinite(lastAt) ? Math.max(0, (nowMs - lastAt) / 1000) : null
  const idleLabel = idle !== null ? `（${approx}閒置 ${idleText(idle)}）` : ''
  const n = pc.recache_tokens_if_cold ?? fallbackTokens
  return {
    kind: 'cold',
    text: `快取已過期${idleLabel}，送出這則會重寫${n !== null ? `約 ${tokensK(n)}` : '整段 context'}`,
    short: `快取已過期，送出重寫${n !== null ? ` ${tokensK(n)}` : '整段'}`,
  }
}

export function composerCostHint(input: ComposerCostInput): ComposerCostHint {
  const { kind, status, lastApiAt, ttlSecs, nowMs, working } = input
  // grok（與其他 kind）完全不管。
  if (kind !== 'claude' && kind !== 'codex') return { context: null, cache: null }
  const pc = input.promptCache ?? status?.prompt_cache ?? null
  const tokens = contextTokens(status, pc)
  const approx = tokens !== null ? `約 ${tokensK(tokens)}` : null
  const pct = pc?.context_used_pct ?? status?.context_used_pct ?? null
  const context = pct !== null ? `context ${pctText(pct)}${approx ? `（${approx}）` : ''}` : null

  let cache: ComposerCostHint['cache'] = null
  const viaPc = pc ? fromPromptCache(pc, input, tokens) : undefined
  if (viaPc !== undefined) {
    cache = viaPc
  } else {
    const cs = cacheState(lastApiAt, ttlSecs, nowMs, working)
    if (cs) {
      if (working) {
        cache = { kind: 'hot', text: '快取還熱（回合進行中）', short: '快取還熱' }
      } else if (cs.level === 'cold') {
        const idle = idleText((nowMs - Date.parse(lastApiAt ?? '')) / 1000)
        cache = {
          kind: 'cold',
          text: `快取已過期（閒置 ${idle}），送出這則會重新讀入整段 context${approx ? `（${approx}）` : ''}`,
          short: `快取已過期，送出重讀${tokens !== null ? ` ${tokensK(tokens)}` : ''}`,
        }
      } else {
        const mins = Math.max(1, Math.ceil(cs.remainingSecs / 60))
        cache = { kind: 'hot', text: `快取還熱（剩 ${mins} 分）`, short: `快取熱 剩 ${mins} 分` }
      }
    }
  }
  return { context, cache }
}
