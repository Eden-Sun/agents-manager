import type { KindQuota, QuotaWindow } from '../api/types'

export function disableUntil(q: KindQuota | null | undefined, now: number): number | null {
  const windows = [q?.five_hour, q?.seven_day, q?.fable].filter((w): w is QuotaWindow => !!w)
  if (!windows.length) return null
  if (!windows.some((w) => w.critical || w.low)) {
    const known = windows.map((w) => futureMs(w, now)).filter((t): t is number => t !== null)
    return known.length ? Math.min(...known) : null
  }
  const blocking = windows.some((w) => w.critical)
    ? windows.filter((w) => w.critical)
    : windows.filter((w) => w.low)
  const resets = blocking.map((w) => futureMs(w, now))
  if (resets.includes(null)) return null
  return Math.max(...(resets as number[]))
}

function futureMs(w: QuotaWindow, now: number): number | null {
  if (!w.resets_at) return null
  const t = Date.parse(w.resets_at)
  return Number.isNaN(t) || t <= now ? null : t
}
