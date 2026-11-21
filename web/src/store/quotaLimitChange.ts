import type { KindQuota, QuotaMap } from '../api/types'

function limitHitDiffers(prev: KindQuota | null, next: KindQuota | null): boolean {
  const a = prev?.limit_hit ?? null
  const b = next?.limit_hit ?? null
  if (a === null || b === null) return a !== b
  return a.at !== b.at || a.until !== b.until || a.message !== b.message
}

export function quotaLimitChanged(before: QuotaMap, entries: readonly (readonly [string, KindQuota | null])[]): boolean {
  return entries.some(([key, next]) => {
    const prev = before[key]
    return prev !== undefined && limitHitDiffers(prev, next)
  })
}
