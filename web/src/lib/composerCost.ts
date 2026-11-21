import { cacheLevel, cacheState } from './cacheClock'
import { solWindowApplies } from './codexContextWindow'
import type { PromptCacheInfo } from '../api/types'
import { t } from './i18n'

export interface CostStatus {
  context_used_pct: number | null
  context_used_tokens: number | null
  context_size: number | null
  prompt_cache?: PromptCacheInfo | null
  context_unknown?: boolean
}

export interface CacheInput {
  kind: string | null | undefined
  promptCache?: PromptCacheInfo | null
  model?: string | null
  status: CostStatus | null | undefined
  lastApiAt: string | null | undefined
  keptWarmAt?: string | null
  ttlSecs: number | null | undefined
  apiLastTokens?: number | null
  nowMs: number
  working: boolean
}

export interface CacheView {
  state: 'hot' | 'cold'
  mins: number | null
  working: boolean
  approx: boolean
  rewriteTokens: number | null
  contextUnknown?: boolean
}

export function tokensK(n: number): string {
  if (n >= 999_500) {
    const m = Math.round(n / 100_000) / 10
    return `${m >= 10 || Number.isInteger(m) ? Math.round(m) : m.toFixed(1)}M`
  }
  if (n >= 1000) return `${Math.round(n / 1000)}K`
  return String(Math.round(n))
}

export function tokensLower(n: number): string {
  return tokensK(n).replace('K', 'k')
}

export function compactTokens(n: number): string {
  if (n >= 999_500) {
    const m = Math.round(n / 100_000) / 10
    return `${m >= 10 || Number.isInteger(m) ? Math.round(m) : m.toFixed(1)}M`
  }
  if (n >= 1000) return `${Math.round(n / 1000)}k`
  return String(n)
}

function pctText(n: number): string {
  const r = Math.round(n * 10) / 10
  return `${Number.isInteger(r) ? r : r.toFixed(1)}%`
}

export function contextTokens(s: CostStatus | null | undefined, pc?: PromptCacheInfo | null): number | null {
  if (pc?.context_used_tokens != null && pc.context_used_tokens > 0) return pc.context_used_tokens
  const pct = pc?.context_used_pct ?? s?.context_used_pct ?? null
  const size = pc?.context_size ?? s?.context_size ?? null
  if (pct !== null && size !== null && size > 0) return (pct / 100) * size
  if (s && s.context_used_tokens !== null && s.context_used_tokens > 0) return s.context_used_tokens
  return null
}

export function contextFallback(input: CacheInput): { pct: string; detail: string | null } | null {
  if (input.kind !== 'claude' && input.kind !== 'codex') return null
  if (input.status?.context_used_pct != null) return null
  const pc = input.promptCache ?? input.status?.prompt_cache ?? null
  if (solWindowApplies(input.kind, input.model)) return null
  if (pc?.context_used_pct == null) return null
  const used = contextTokens(input.status, pc)
  const detail = used !== null && pc.context_size ? `${tokensLower(used)}/${tokensLower(pc.context_size)}` : null
  return { pct: pctText(pc.context_used_pct), detail }
}

function fromPromptCache(pc: PromptCacheInfo, input: CacheInput, fallbackTokens: number | null): CacheView | undefined {
  const { nowMs, working } = input
  if (pc.warm === null && pc.expires_at === null) return undefined
  const approx = pc.source === 'rollout_estimate'
  if (working) return { state: 'hot', mins: null, working: true, approx, rewriteTokens: null }
  const exp = pc.expires_at !== null ? (pc.expires_at > 1e12 ? pc.expires_at : pc.expires_at * 1000) : null
  const remaining = exp !== null ? Math.round((exp - nowMs) / 1000) : null
  if (pc.warm !== false && (remaining === null || remaining > 0)) {
    return { state: 'hot', mins: remaining === null ? null : Math.max(1, Math.ceil(remaining / 60)), working: false, approx, rewriteTokens: null }
  }
  return { state: 'cold', mins: null, working: false, approx, rewriteTokens: pc.recache_tokens_if_cold ?? fallbackTokens }
}

export function cacheView(input: CacheInput): CacheView | null {
  const { kind, status, lastApiAt, ttlSecs, nowMs, working } = input
  if (kind !== 'claude' && kind !== 'claude-api') return null
  const pc = input.promptCache ?? status?.prompt_cache ?? null
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

export function cacheLabel(v: CacheView): string {
  if (v.state === 'cold') return v.approx ? t('已冷（約）') : t('已冷')
  if (v.mins === null) return t('熱')
  return v.approx
    ? t('熱（約剩 {n} 分）', { n: v.mins })
    : t('熱（剩 {n} 分）', { n: v.mins })
}

export function coldWarning(v: CacheView | null): string | null {
  if (!v || v.working) return null
  if (v.state === 'hot' && v.mins !== null && v.mins <= 2) {
    return t('快取快過期，繼續工作就送出／可先壓縮')
  }
  if (v.state !== 'cold') return null
  return v.rewriteTokens !== null
    ? t('快取已過期，送出這則會重寫約 {tokens}', { tokens: tokensK(v.rewriteTokens) })
    : v.contextUnknown
      ? t('快取已過期；剛壓縮或接回過，context 大小下一回合才知道')
      : t('快取已過期，送出這則會重寫整段 context')
}

const API_WRITE_USD_PER_M: Record<string, { m5: number; h1: number }> = {
  'claude-opus-5-5': { m5: 5, h1: 8 },
  'claude-fable-5-2': { m5: 10, h1: 16 },
}

export function apiRewriteUsd(tokens: number, model: string | null | undefined, ttlSecs: number | null | undefined, fast: boolean): number | null {
  const rate = model ? API_WRITE_USD_PER_M[model] : undefined
  return rate ? (tokens / 1_000_000) * (ttlSecs === 3600 ? rate.h1 : rate.m5) * (fast ? 2 : 1) : null
}

export function clockText(secs: number): string {
  const s = Math.max(0, Math.floor(secs))
  const p = (n: number) => String(n).padStart(2, '0')
  const h = Math.floor(s / 3600)
  return h > 0 ? `${h}:${p(Math.floor((s % 3600) / 60))}:${p(s % 60)}` : `${Math.floor(s / 60)}:${p(s % 60)}`
}

export interface ApiCacheInfo { last_request_at: string | null; ttl_secs: number | null; expires_at: string | null; working: boolean }

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

export function apiColdWarning(v: CacheView | null, model: string | null | undefined, ttlSecs: number | null | undefined, fast: boolean, lastTokens?: number | null): string | null {
  if (!v || v.working) return null
  const cold = v.state === 'cold'
  const expiring = v.state === 'hot' && v.mins !== null && v.mins <= 2
  if (!cold && !expiring) return null
  const rate = model ? API_WRITE_USD_PER_M[model] : undefined
  const tokens = v.rewriteTokens ?? lastTokens ?? null
  const lead = cold ? t('快取已冷') : t('快取快過期，繼續工作就送出／可先壓縮')
  if (tokens === null) return t('{lead}：下一則會重寫整段 context', { lead })
  if (!rate) return t('{lead}：下一則約重寫 {tokens} token', { lead, tokens: tokensK(tokens) })
  const usd = apiRewriteUsd(tokens, model, ttlSecs, fast)!
  return t('{lead}：下一則約重寫 {tokens} token（約 ${cost}）', { lead, tokens: tokensK(tokens), cost: usd.toFixed(2) })
}
