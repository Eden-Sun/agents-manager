import { t } from './i18n'

export const RESET_SOON_MS = 3 * 60 * 60 * 1000
export const RESET_SOON_WEEKLY_MS = 24 * 60 * 60 * 1000
export const LOW_WEEKLY_PCT = 10

export interface ResetRule {
  soonMs: number
  belowPct: number | null
}
export const RULE_5H: ResetRule = { soonMs: RESET_SOON_MS, belowPct: null }
export const RULE_WEEKLY: ResetRule = { soonMs: RESET_SOON_WEEKLY_MS, belowPct: LOW_WEEKLY_PCT }

function short(ms: number): string {
  const m = Math.max(0, Math.ceil(ms / 60_000))
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
}

export function msUntil(iso: string | null | undefined, now: number): number | null {
  if (!iso) return null
  const t = new Date(iso).getTime()
  return Number.isNaN(t) ? null : t - now
}

export function resetBadge(
  remainingPct: number | null | undefined,
  resetsAt: string | null | undefined,
  now: number,
  rule: ResetRule = RULE_5H,
): string | null {
  if (remainingPct === null || remainingPct === undefined) return null
  if (rule.belowPct === null ? remainingPct > 0 : remainingPct >= rule.belowPct) return null
  if (!resetsAt) return null
  const t = new Date(resetsAt).getTime()
  if (Number.isNaN(t)) return null
  const left = t - now
  if (left <= 0 || left > rule.soonMs) return null
  return short(left)
}

export function fmtLeft(ms: number): string {
  if (ms <= 0) return t('即將重置')
  const m = Math.ceil(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return `${d}d${h % 24}h`
  if (h > 0) return `${h}h${m % 60}m`
  return `${m}m`
}

export function fmtLeftLong(ms: number): string {
  if (ms <= 0) return t('即將重置')
  const m = Math.ceil(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return t('{n} 天 {h} 小時', { n: d, h: h % 24 })
  if (h > 0) return t('{n} 小時 {m} 分', { n: h, m: m % 60 })
  return t('{n} 分', { n: m })
}
