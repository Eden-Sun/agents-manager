import type { KindQuota, QuotaMap } from '../api/types'

/**
 * 撞限（`limit_hit`）有沒有變：只要 null／非 null、`at`、`until`、`message` 任一不同就算。
 * 側欄的「到限」晶片在 `bot.limit_hit`（`/api/state`），quota 事件本身帶不到它，所以這裡只決定要不要重抓（#1899）。
 */
function limitHitDiffers(prev: KindQuota | null, next: KindQuota | null): boolean {
  const a = prev?.limit_hit ?? null
  const b = next?.limit_hit ?? null
  if (a === null || b === null) return a !== b
  return a.at !== b.at || a.until !== b.until || a.message !== b.message
}

/**
 * 收到 `quota_updated` 的一批（單筆或整包 `{kinds}` 都先攤平成 `[完整 map key, 新讀數]`）裡，
 * 有沒有哪一把 key 的撞限變了。**沒有先前快取的 key 不算**：第一次見到不因此重抓。
 */
export function quotaLimitChanged(before: QuotaMap, entries: readonly (readonly [string, KindQuota | null])[]): boolean {
  return entries.some(([key, next]) => {
    const prev = before[key]
    return prev !== undefined && limitHitDiffers(prev, next)
  })
}
