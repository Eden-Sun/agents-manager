import { msg, t } from './i18n'

/**
 * 主力晶片的 prompt cache 倒數（SPEC §6.5j）：晶片原本的底色就是倒數，不另加元素、不改尺寸。
 *
 * daemon 在 run 上帶 `last_api_at`（最後一次 API 活動）與 `cache_ttl_secs`（claude 3600；codex 不因閒置冷掉、grok 不明，都不帶）；
 * 這裡只做算術：剩餘＝`last_api_at + ttl - now`，回合進行中＝滿。網頁每 15 秒重算一次（`CACHE_TICK_MS`）；
 * 紅色時每 5 秒（`CACHE_RED_TICK_MS`）：紅色那級的底色改以 5 分鐘為滿刻度重算，見 `frac`。
 * 主力的 prompt cache「保溫」（`primary_keepalive`，SPEC §6.5k）：`cache_kept_warm_at` 是保溫／熱壓讓 cache 實際變熱的時間，
 * 顏色與剩餘從 `max(last_api_at, cache_kept_warm_at)` 起算；`last_api_at` 仍是真實年齡（保溫不算活動，只在 tooltip 講）。
 * 門檻：剩 > 15 分鐘綠、5–15 分鐘黃、< 5 分鐘紅、到期＝「已涼」（回到一般底色，只在 tooltip 講）。
 */

export const CACHE_TICK_MS = 15_000
/** 有主力在紅色（< 5 分）時的重算間隔：紅色只剩 5 分鐘，15 秒一跳看不出流失（使用者 2026-10-14）。 */
export const CACHE_RED_TICK_MS = 5_000

export type CacheLevel = 'fresh' | 'warn' | 'low' | 'cold'

export interface CacheState {
  level: CacheLevel
  /** 剩餘秒數（≥ 0）。 */
  remainingSecs: number
  /**
   * 底色由左往右填的比例，0–1，只拿來畫寬度（不是數字，tooltip 的「約 N 分」照剩餘秒數算）。
   * 綠、黃＝剩餘／TTL；紅（`level === 'low'`，與剩餘 ≤ 5 分鐘）＝剩餘／5 分鐘，剛變紅時是滿的、5 分鐘內流到 0。
   * 「剩 300 秒」這一秒仍照黃的 `level`，但比例已按紅色算（滿條），免得從黃轉紅時底色跳一下。
   */
  frac: number
  /** tooltip 用的一句話。 */
  title: string
}

const WARN_SECS = 15 * 60
const LOW_SECS = 5 * 60

export function cacheLevel(remainingSecs: number): CacheLevel {
  if (remainingSecs <= 0) return 'cold'
  if (remainingSecs < LOW_SECS) return 'low'
  if (remainingSecs <= WARN_SECS) return 'warn'
  return 'fresh'
}

function clock(t: Date, nowMs: number): string {
  const p = (n: number) => String(n).padStart(2, '0')
  const time = `${p(t.getHours())}:${p(t.getMinutes())}`
  const now = new Date(nowMs)
  const sameDay = t.getFullYear() === now.getFullYear() && t.getMonth() === now.getMonth() && t.getDate() === now.getDate()
  if (sameDay) return time
  // 跨年要帶年份：去年 12 月底的活動在 1 月看不出是哪一年（tooltip 寫的是「上次活動」）。
  const head = t.getFullYear() === now.getFullYear() ? '' : `${t.getFullYear()}/`
  return `${head}${t.getMonth() + 1}/${t.getDate()} ${time}`
}

/**
 * 算一顆 bot 現在的快取狀態；TTL 不明（grok、舊 daemon）或沒有任何活動紀錄回 `null`（不畫）。
 * `working`＝回合進行中，一律滿條。
 */
export function cacheState(
  lastApiAt: string | null | undefined,
  ttlSecs: number | null | undefined,
  nowMs: number,
  working = false,
  keptWarmAt?: string | null,
): CacheState | null {
  if (!ttlSecs || ttlSecs <= 0) return null
  const at = lastApiAt ? Date.parse(lastApiAt) : NaN
  if (working) {
    const when = Number.isFinite(at) ? t('（上次活動 {time}）', { time: clock(new Date(at), nowMs) }) : ''
    return { level: 'fresh', remainingSecs: ttlSecs, frac: 1, title: t('回合進行中，快取是熱的{when}', { when }) }
  }
  if (!Number.isFinite(at)) return null
  // 保溫過：cache 從保溫那一刻起算熱；數字（上次活動）仍是真實年齡。
  const kept = keptWarmAt ? Date.parse(keptWarmAt) : NaN
  const hotSince = Number.isFinite(kept) && kept > at ? kept : at
  // 時鐘差一點（daemon 比這台快）時不讓剩餘超過 TTL。
  const remainingSecs = Math.max(0, Math.min(ttlSecs, Math.round((hotSince + ttlSecs * 1000 - nowMs) / 1000)))
  const level = cacheLevel(remainingSecs)
  const last = hotSince > at
    ? t('上次活動 {time}，已保溫 {warm}', { time: clock(new Date(at), nowMs), warm: clock(new Date(hotSince), nowMs) })
    : t('上次活動 {time}', { time: clock(new Date(at), nowMs) })
  const title = level === 'cold'
    ? t('快取已涼（{last}）', { last })
    : t('快取約 {n} 分後到期（{last}）', { n: Math.max(1, Math.ceil(remainingSecs / 60)), last })
  // 紅色（≤ 5 分）改以 5 分鐘為滿刻度，見 `CacheState.frac`；綠、黃照舊。
  const frac = remainingSecs <= LOW_SECS ? Math.min(1, remainingSecs / LOW_SECS) : remainingSecs / ttlSecs
  return { level, remainingSecs, frac, title }
}

/** 「不用保溫」（冰凍）時晶片 tooltip／狀態卡講的一句話（不顯示快取倒數；context 夠大時 cache 到期前會自動壓縮一次，SPEC §6.5k）。 */
export const NO_WARM_TEXT = msg('不保溫：cache 到期前會自動壓縮（context 夠大時）')

/**
 * 主力晶片用的快取狀態：使用者已按「不用保溫」（`run.keep_warm_skip`）就不畫倒數底色（回 `null`、晶片回一般樣式），
 * 取消後（daemon 廣播 `keep_warm_skip=false`）恢復；其餘照 `cacheState`。
 */
export function chipCache(
  run: { last_api_at?: string | null; cache_ttl_secs?: number | null; cache_kept_warm_at?: string | null; keep_warm_skip?: boolean } | null | undefined,
  nowMs: number,
  working = false,
): CacheState | null {
  if (run?.keep_warm_skip === true) return null
  return cacheState(run?.last_api_at, run?.cache_ttl_secs, nowMs, working, run?.cache_kept_warm_at)
}
