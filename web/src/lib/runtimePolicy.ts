/**
 * 角色 policy 與 bot 執行期覆寫（B 期）的前端規則。形狀與 daemon `am-base::runtime_policy` 一致；
 * 欄位 `null`/缺＝跟上一層（角色）或預設。驗證以 daemon 為準，這裡只做輸入範圍與「跟預設」的整理。
 */
import type { RuntimeSettings } from '../api/types'

export const RUNTIME_ROLES = ['manager', 'specialist', 'advisor', 'executor'] as const
export type RuntimeRole = (typeof RUNTIME_ROLES)[number]

/** context 拉條的刻度（token）。上限 1M＝角色政策的上限；實際套用時夾到該 bot 的模型視窗。 */
export const CONTEXT_TICKS = [200_000, 400_000, 512_000, 1_000_000] as const
export const KEEP_WARM_MIN = 5
export const KEEP_WARM_MAX = 59
export const HOT_COMPACT_MIN = 5
export const HOT_COMPACT_MAX = 119
export const HOT_PCT_MAX = 100

/** 「加入建議」帶入的值（只帶入草稿，不會自動儲存）。 */
export const ROLE_SUGGESTIONS: Record<RuntimeRole, RuntimeSettings> = {
  manager: { keep_warm: true },
  specialist: { context_tokens: 512_000, keep_warm: true },
  advisor: { keep_warm: false },
  executor: { context_tokens: 200_000, keep_warm: false },
}

/** 去掉 `null`／`undefined`／空字串的欄位（送出時只帶有值的欄位；全部沒有＝空物件）。 */
export function cleanSettings(s: RuntimeSettings): RuntimeSettings {
  const out: Record<string, unknown> = {}
  for (const [k, v] of Object.entries(s)) {
    if (v === null || v === undefined || v === '') continue
    out[k] = v
  }
  return out as RuntimeSettings
}

/** 把 context 夾到拉條刻度內（非刻度值取最近的一格）。 */
export function nearestTickIndex(tokens: number | null | undefined): number {
  if (tokens === null || tokens === undefined) return 0
  let best = 0
  for (let i = 1; i < CONTEXT_TICKS.length; i++) {
    if (Math.abs(CONTEXT_TICKS[i] - tokens) < Math.abs(CONTEXT_TICKS[best] - tokens)) best = i
  }
  return best
}

/** 1000000 → "1M"、512000 → "512k"。 */
export function formatContext(tokens: number): string {
  if (tokens >= 1_000_000) return `${tokens / 1_000_000}M`
  return `${Math.round(tokens / 1000)}k`
}

/** 數字欄位：空白或非數字回 null（＝跟預設）；超出範圍的仍回原值，讓 daemon 給出準確的 400。 */
export function numberOrNull(raw: string): number | null {
  const t = raw.trim()
  if (t === '') return null
  const n = Number(t)
  return Number.isFinite(n) ? Math.trunc(n) : null
}

/** 範圍檢查（前端先擋明顯錯誤；儲存鈕據此停用）。回傳第一個錯誤的欄位鍵，沒有就 null。 */
export function firstRangeError(s: RuntimeSettings): keyof RuntimeSettings | null {
  const inRange = (v: number | null | undefined, lo: number, hi: number) => v === null || v === undefined || (v >= lo && v <= hi)
  if (!inRange(s.keep_warm_after_min, KEEP_WARM_MIN, KEEP_WARM_MAX)) return 'keep_warm_after_min'
  if (!inRange(s.hot_compact_after_min, HOT_COMPACT_MIN, HOT_COMPACT_MAX)) return 'hot_compact_after_min'
  if (!inRange(s.hot_compact_min_pct, 0, HOT_PCT_MAX)) return 'hot_compact_min_pct'
  if (s.keep_warm_after_min != null && s.hot_compact_after_min != null && s.hot_compact_after_min <= s.keep_warm_after_min) {
    return 'hot_compact_after_min'
  }
  return null
}

/** 兩份設定是否相同（清理後比較；順序無關）。 */
export function sameSettings(a: RuntimeSettings, b: RuntimeSettings): boolean {
  const x = cleanSettings(a) as Record<string, unknown>
  const y = cleanSettings(b) as Record<string, unknown>
  const keys = new Set([...Object.keys(x), ...Object.keys(y)])
  for (const k of keys) if (x[k] !== y[k]) return false
  return true
}
