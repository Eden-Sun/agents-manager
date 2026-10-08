import type { KindQuota, QuotaWindow } from '../api/types'

/**
 * 額度格「暫時停用」的自動解除時刻（issue #920）。
 *
 * 要等的是**用完的那個窗口**的 reset，不是三個窗口裡最早的：7d 歸零、5h 還有時，最早的 reset 是 5h 的，
 * 取它的話 5h 一到 bot 就被放回側欄，額度其實還是 0。
 * - 有 `critical` 的窗口：取它們之中最晚的 reset（5h 與 7d 都 critical，7d 回來才算真的有額度）；
 * - 沒有 critical 但有 `low`：同樣取 low 窗口中最晚的；
 * - 都沒有（使用者只是想先收起來）：最早的 reset；
 * - 沒有任何還沒到的 `resets_at`：`null`，只能手動解除。
 * 已經過去或讀不懂的 `resets_at` 不算。門檻（low／critical）一律照 daemon 的旗標，不自己用 `used_pct` 判斷。
 */
export function disableUntil(q: KindQuota | null | undefined, now: number): number | null {
  const future: { t: number; w: QuotaWindow }[] = []
  for (const w of [q?.five_hour, q?.seven_day, q?.fable]) {
    if (!w?.resets_at) continue
    const t = Date.parse(w.resets_at)
    if (Number.isNaN(t) || t <= now) continue
    future.push({ t, w })
  }
  if (!future.length) return null
  const latest = (xs: { t: number }[]) => Math.max(...xs.map((x) => x.t))
  const critical = future.filter((x) => x.w.critical)
  if (critical.length) return latest(critical)
  const low = future.filter((x) => x.w.low)
  if (low.length) return latest(low)
  return Math.min(...future.map((x) => x.t))
}
