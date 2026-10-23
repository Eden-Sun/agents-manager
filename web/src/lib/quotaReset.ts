import { t } from './i18n'

/**
 * 「這一格已經沒額度了，還要等多久」——用完的視窗改寫倒數（2026-09-14 使用者：先要「重置時刻
 * ＋倒數」，同日再改成「不需要重置時間，只要 xhym 就好」——鐘點要自己換算，倒數直接就是答案）。
 *
 * 為什麼只在這兩個條件下才寫：額度還有的時候，人要看的是「還剩多少」；歸零之後那個數字不再變，
 * 唯一有用的資訊變成「什麼時候能再跑」。而重置在三小時以外時寫個鐘點也沒有意義（那是明天的事，
 * 量表旁邊的刻度已經說了），所以門檻設在三小時內——那是「等一下就回來、值得等」的範圍。
 */
export const RESET_SOON_MS = 3 * 60 * 60 * 1000
/**
 * 週窗口（7d／週／Fable）改成 24 小時內就寫倒數（2026-09-17 使用者：「7d 是 < 24hr 就出倒數字」）：
 * 週額度用完要等的是天，「三小時內」幾乎碰不到；剩不到一天就已經是「明天就回來、值得等」的範圍。
 */
export const RESET_SOON_WEEKLY_MS = 24 * 60 * 60 * 1000
/** 週窗口不必等到 0%：剩不到 10% 就寫倒數（同日使用者：「低於 10% 也 show」）——快見底時要看的也是還要撐多久。 */
export const LOW_WEEKLY_PCT = 10

/** 各窗口的倒數規則：5h 用完且三小時內；週窗口剩不到 10% 且 24 小時內。 */
export interface ResetRule {
  soonMs: number
  /** 剩餘百分比嚴格小於這個值才寫倒數；`0` 以下（含）才算＝用完才寫。 */
  belowPct: number | null
}
export const RULE_5H: ResetRule = { soonMs: RESET_SOON_MS, belowPct: null }
export const RULE_WEEKLY: ResetRule = { soonMs: RESET_SOON_WEEKLY_MS, belowPct: LOW_WEEKLY_PCT }

/**
 * `2h42` / `0h42`——使用者 2026-09-14：「不要 m」。一律 `<小時>h<分>`，不到一小時寫 `0h42`
 * 而不是裸的數字：那一格旁邊全是百分比，只寫 `42` 會被讀成 42%。
 */
function short(ms: number): string {
  const m = Math.max(0, Math.ceil(ms / 60_000))
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}`
}

/**
 * 距離 `iso` 還有幾毫秒。沒有時間、或字串讀不出來＝`null`：呼叫端照額度列的空狀態顯示「—」、不掛提示，
 * 不能把 `NaN` 印出來（`new Date("…").getTime()` 讀不出來時是 NaN，直接算下去會寫成「NaN 分」）。
 */
export function msUntil(iso: string | null | undefined, now: number): number | null {
  if (!iso) return null
  const t = new Date(iso).getTime()
  return Number.isNaN(t) ? null : t - now
}

/**
 * 這一格現在要不要寫倒數？`remainingPct` 是**剩餘**百分比（跟量表同一個數字）。
 *
 * 回 `null` 的情況：還有額度、沒有重置時間、時間讀不出來、或重置在三小時之外。已經過了重置時刻
 * （下一次輪詢還沒把數字更新回來）也回 `null`：那時候該說的是「回來了」，不是倒數負數。
 */
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

/** `2h13m` / `4d0h` / `12m`——和狀態列同一種寫法。 */
export function fmtLeft(ms: number): string {
  if (ms <= 0) return t('即將重置')
  // 無條件進位（同 resetBadge／soonLabel）：剩 30 秒寫 1m，不寫 0m。
  const m = Math.ceil(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return `${d}d${h % 24}h`
  if (h > 0) return `${h}h${m % 60}m`
  return `${m}m`
}

/** popover 用的長寫法（`2 天 3 小時`）；`fmtLeft` 是窄處的緊縮版。 */
export function fmtLeftLong(ms: number): string {
  if (ms <= 0) return t('即將重置')
  // 同 fmtLeft：不到一分鐘寫「1 分」，不寫「0 分」。
  const m = Math.ceil(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return t('{n} 天 {h} 小時', { n: d, h: h % 24 })
  if (h > 0) return t('{n} 小時 {m} 分', { n: h, m: m % 60 })
  return t('{n} 分', { n: m })
}
