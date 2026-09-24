/**
 * 額度量表上「這一格是不是沿用上一份的讀數」（issue #540）。
 *
 * daemon 對**這一次真的帶進來**的窗會把 `observed_at` 蓋成這筆讀數的 `updated_at`，新讀數缺的那一桶
 * 則原樣保留舊的 `observed_at`（`daemon/src/quota.rs`：「沒有這一步，被沿用的那一桶會跟著 `updated_at`
 * 一直『看起來很新』」）。所以「`observed_at` 比 `updated_at` 舊」就是沿用——不必另外定一套門檻。
 *
 * 整筆的 `stale` 是另一件事：那只在開機從快取回填時為 true，沿用舊桶時是 false。
 */
import type { QuotaWindow } from '../api/types'

/** 這一桶是沿用的就回它自己的觀測時間（ISO），否則 `null`。時間解不開一律回 `null`：不知道就不要亂標。 */
export function carriedOverAt(w: Pick<QuotaWindow, 'observed_at'> | null | undefined, updatedAt: string | null | undefined): string | null {
  const observed = w?.observed_at
  if (!observed) return null
  const at = Date.parse(observed)
  const rec = Date.parse(updatedAt ?? '')
  if (Number.isNaN(at) || Number.isNaN(rec)) return null
  return at < rec ? observed : null
}
