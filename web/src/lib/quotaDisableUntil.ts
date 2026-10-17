import type { KindQuota, QuotaWindow } from '../api/types'

/**
 * 額度格「暫時停用」的自動解除時刻（issue #920）。
 *
 * 要等的是**用完的那個窗口**的 reset，不是三個窗口裡最早的：7d 歸零、5h 還有時，最早的 reset 是 5h 的，
 * 取它的話 5h 一到 bot 就被放回側欄，額度其實還是 0。
 * 先辨識阻擋的窗口（reset 有沒有都先看），再看它們的 reset：
 * - 有 `critical` 的窗口：只看 critical 那幾個；沒有 critical 但有 `low`：只看 low 那幾個；
 * - 都沒有（使用者只是想先收起來）：看全部窗口，取最早的 reset；
 * - 被選為阻擋的窗口中，只要有任何一個 `resets_at` 不存在、已經過去或讀不懂，就回 `null`（只能手動解除），
 *   不准改借健康窗口的 reset；全部都是未來的有效時刻才算：critical／low 取最晚（用完的都要回來才算有額度），
 *   都正常取最早。
 * 沒有任何窗口資料也回 `null`。門檻（low／critical）一律照 daemon 的旗標，不自己用 `used_pct` 判斷。
 */
export function disableUntil(q: KindQuota | null | undefined, now: number): number | null {
  const windows = [q?.five_hour, q?.seven_day, q?.fable].filter((w): w is QuotaWindow => !!w)
  if (!windows.length) return null
  const exhausted = windows.some((w) => w.critical || w.low)
  const blocking = windows.some((w) => w.critical)
    ? windows.filter((w) => w.critical)
    : windows.filter((w) => w.low)
  const resets = (exhausted ? blocking : windows).map((w) => futureMs(w, now))
  if (resets.includes(null)) return null
  const known = resets as number[]
  return exhausted ? Math.max(...known) : Math.min(...known)
}

function futureMs(w: QuotaWindow, now: number): number | null {
  if (!w.resets_at) return null
  const t = Date.parse(w.resets_at)
  return Number.isNaN(t) || t <= now ? null : t
}
