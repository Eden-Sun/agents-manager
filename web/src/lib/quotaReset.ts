/**
 * 「這一格已經沒額度了，幾點回來」——用完的視窗要寫出**重置時刻**並倒數（2026-09-14 使用者：
 * 「當已 0 且 reset 天數 < 3 小時，標示 3:25 的重置時間且自動倒數」）。
 *
 * 為什麼只在這兩個條件下才寫：額度還有的時候，人要看的是「還剩多少」；歸零之後那個數字不再變，
 * 唯一有用的資訊變成「什麼時候能再跑」。而重置在三小時以外時寫個鐘點也沒有意義（那是明天的事，
 * 量表旁邊的刻度已經說了），所以門檻設在三小時內——那是「等一下就回來、值得等」的範圍。
 */
export const RESET_SOON_MS = 3 * 60 * 60 * 1000

export interface ResetBadge {
  /** 重置時刻，當地時間的 `3:25`。 */
  at: string
  /** 還要多久，`2h41m` / `12m`；跟量表旁邊那個緊縮寫法一致。 */
  left: string
}

function clock(t: number): string {
  const d = new Date(t)
  return `${d.getHours()}:${String(d.getMinutes()).padStart(2, '0')}`
}

function short(ms: number): string {
  const m = Math.max(0, Math.ceil(ms / 60_000))
  const h = Math.floor(m / 60)
  return h > 0 ? `${h}h${String(m % 60).padStart(2, '0')}m` : `${m}m`
}

/**
 * 這一格現在要不要寫重置時刻？`remainingPct` 是**剩餘**百分比（跟量表同一個數字）。
 *
 * 回 `null` 的情況：還有額度、沒有重置時間、時間讀不出來、或重置在三小時之外。已經過了重置時刻
 * （下一次輪詢還沒把數字更新回來）也回 `null`：那時候該說的是「回來了」，不是倒數負數。
 */
export function resetBadge(remainingPct: number | null | undefined, resetsAt: string | null | undefined, now: number): ResetBadge | null {
  if (remainingPct === null || remainingPct === undefined || remainingPct > 0) return null
  if (!resetsAt) return null
  const t = new Date(resetsAt).getTime()
  if (Number.isNaN(t)) return null
  const left = t - now
  if (left <= 0 || left > RESET_SOON_MS) return null
  return { at: clock(t), left: short(left) }
}
