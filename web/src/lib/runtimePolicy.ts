import type { RuntimeSettings } from '../api/types'

export const RUNTIME_ROLES = ['manager', 'specialist', 'advisor', 'executor'] as const
export type RuntimeRole = (typeof RUNTIME_ROLES)[number]

export const CONTEXT_TICKS = [200_000, 300_000, 400_000, 500_000, 600_000, 800_000, 1_000_000] as const
export const KEEP_WARM_MIN = 5
export const KEEP_WARM_MAX = 59
export const HOT_COMPACT_MIN = 5
export const HOT_COMPACT_MAX = 119
export const HOT_PCT_MAX = 100

export const ROLE_SUGGESTIONS: Record<RuntimeRole, RuntimeSettings> = {
  manager: { keep_warm: true },
  specialist: { context_tokens: 500_000, keep_warm: true },
  advisor: { keep_warm: false },
  executor: { context_tokens: 200_000, keep_warm: false },
}

export function cleanSettings(s: RuntimeSettings): RuntimeSettings {
  const out: Record<string, unknown> = {}
  for (const [k, v] of Object.entries(s)) {
    if (v === null || v === undefined || v === '') continue
    out[k] = v
  }
  return out as RuntimeSettings
}

export function nearestTickIndex(tokens: number | null | undefined): number {
  if (tokens === null || tokens === undefined) return 0
  let best = 0
  for (let i = 1; i < CONTEXT_TICKS.length; i++) {
    if (Math.abs(CONTEXT_TICKS[i] - tokens) < Math.abs(CONTEXT_TICKS[best] - tokens)) best = i
  }
  return best
}

export function formatContext(tokens: number): string {
  if (tokens >= 1_000_000) return `${tokens / 1_000_000}M`
  return `${Math.round(tokens / 1000)}k`
}

export function numberOrNull(raw: string): number | null {
  const t = raw.trim()
  if (t === '') return null
  const n = Number(t)
  return Number.isFinite(n) ? Math.trunc(n) : null
}

export function firstRangeError(s: RuntimeSettings): keyof RuntimeSettings | null {
  const inRange = (v: number | null | undefined, lo: number, hi: number) => v === null || v === undefined || (v >= lo && v <= hi)
  if (!inRange(s.keep_warm_after_min, KEEP_WARM_MIN, KEEP_WARM_MAX)) return 'keep_warm_after_min'
  if (!inRange(s.hot_compact_after_min, HOT_COMPACT_MIN, HOT_COMPACT_MAX)) return 'hot_compact_after_min'
  if (!inRange(s.hot_compact_min_pct, 0, HOT_PCT_MAX)) return 'hot_compact_min_pct'
  if (s.keep_warm_after_min != null && s.hot_compact_after_min != null && s.hot_compact_after_min <= s.keep_warm_after_min) {
    return 'hot_compact_after_min'
  }
  return null
}

export function sameSettings(a: RuntimeSettings, b: RuntimeSettings): boolean {
  const x = cleanSettings(a) as Record<string, unknown>
  const y = cleanSettings(b) as Record<string, unknown>
  const keys = new Set([...Object.keys(x), ...Object.keys(y)])
  for (const k of keys) if (x[k] !== y[k]) return false
  return true
}
