import type { KindQuota, QuotaWindow } from '../api/types'

/**
 * 額度格「暫時停用」的自動解除時刻（issue #920）。
 *
 * 要等的是**用完的那個窗口**的 reset，不是三個窗口裡最早的：7d 歸零、5h 還有時，最早的 reset 是 5h 的，
 * 取它的話 5h 一到 bot 就被放回側欄，額度其實還是 0。
 * - 有 `critical` 的窗口：只看 critical 那幾個，取最晚的 reset（5h 與 7d 都 critical，7d 回來才算真的有額度）；
 *   沒有 critical 但有 `low`：同樣只看 low 那幾個，取最晚的；
 * - 被選為阻擋的窗口中，只要有任何一個 `resets_at` 不存在、已經過去或讀不懂，就回 `null`（只能手動解除），
 *   不准改借健康窗口的 reset；
 * - 都沒有 critical／low（使用者只是想先收起來）：忽略 reset 缺失、已過去或讀不懂的窗口，取剩下有效的最早；
 *   一個有效的都沒有才回 `null`。
 * 沒有任何窗口資料也回 `null`。門檻（low／critical）一律照 daemon 的旗標，不自己用 `used_pct` 判斷。
 */
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
