/**
 * 主力晶片的 prompt cache 倒數（SPEC §6.5j）：晶片原本的底色就是倒數，不另加元素、不改尺寸。
 *
 * daemon 在 run 上帶 `last_api_at`（最後一次 API 活動）與 `cache_ttl_secs`（claude／codex 3600，grok 不帶）；
 * 這裡只做算術：剩餘＝`last_api_at + ttl - now`，回合進行中＝滿。網頁每 15 秒重算一次（`CACHE_TICK_MS`）。
 * 主力的 prompt cache 續命（`primary_keepalive`，SPEC §6.5k）：`cache_kept_alive_at` 是續命／到點壓縮讓 cache 實際變熱的時間，
 * 顏色與剩餘從 `max(last_api_at, cache_kept_alive_at)` 起算；`last_api_at` 仍是真實年齡（續命不算活動，只在 tooltip 講）。
 * 門檻：剩 > 15 分鐘綠、5–15 分鐘黃、< 5 分鐘紅、到期＝「已涼」（回到一般底色，只在 tooltip 講）。
 */

export const CACHE_TICK_MS = 15_000

export type CacheLevel = 'fresh' | 'warn' | 'low' | 'cold'

export interface CacheState {
  level: CacheLevel
  /** 剩餘秒數（≥ 0）。 */
  remainingSecs: number
  /** 剩餘佔 TTL 的比例，0–1；底色由左往右填這麼多。 */
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

function hhmm(t: Date): string {
  return `${String(t.getHours()).padStart(2, '0')}:${String(t.getMinutes()).padStart(2, '0')}`
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
  keptAliveAt?: string | null,
): CacheState | null {
  if (!ttlSecs || ttlSecs <= 0) return null
  const at = lastApiAt ? Date.parse(lastApiAt) : NaN
  if (working) {
    const when = Number.isFinite(at) ? `（上次活動 ${hhmm(new Date(at))}）` : ''
    return { level: 'fresh', remainingSecs: ttlSecs, frac: 1, title: `回合進行中，快取是熱的${when}` }
  }
  if (!Number.isFinite(at)) return null
  // 續命過：cache 從續命那一刻起算熱；數字（上次活動）仍是真實年齡。
  const kept = keptAliveAt ? Date.parse(keptAliveAt) : NaN
  const hotSince = Number.isFinite(kept) && kept > at ? kept : at
  // 時鐘差一點（daemon 比這台快）時不讓剩餘超過 TTL。
  const remainingSecs = Math.max(0, Math.min(ttlSecs, Math.round((hotSince + ttlSecs * 1000 - nowMs) / 1000)))
  const level = cacheLevel(remainingSecs)
  const last = hotSince > at ? `上次活動 ${hhmm(new Date(at))}，已續命 ${hhmm(new Date(hotSince))}` : `上次活動 ${hhmm(new Date(at))}`
  const title = level === 'cold' ? `快取已涼（${last}）` : `快取約 ${Math.max(1, Math.ceil(remainingSecs / 60))} 分後到期（${last}）`
  return { level, remainingSecs, frac: remainingSecs / ttlSecs, title }
}
