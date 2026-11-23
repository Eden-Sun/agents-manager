import { msg, t } from './i18n'
import { userTimeZone, zonedParts } from './userTime'

export const CACHE_TICK_MS = 15_000
export const CACHE_RED_TICK_MS = 5_000

export type CacheLevel = 'fresh' | 'warn' | 'low' | 'cold'

export interface CacheState {
  level: CacheLevel
  remainingSecs: number
  frac: number
  title: string
}

const WARN_SECS = 15 * 60
const LOW_SECS = 5 * 60

export function cacheLevel(remainingSecs: number, _ttlSecs?: number): CacheLevel {
  if (remainingSecs <= 0) return 'cold'
  if (remainingSecs < LOW_SECS) return 'low'
  if (remainingSecs <= WARN_SECS) return 'warn'
  return 'fresh'
}

function clock(t: Date, nowMs: number): string {
  const p = (n: number) => String(n).padStart(2, '0')
  const z = zonedParts(t, userTimeZone())
  const time = `${p(z.hour)}:${p(z.minute)}`
  const now = zonedParts(nowMs, userTimeZone())
  const sameDay = z.year === now.year && z.month === now.month && z.day === now.day
  if (sameDay) return time
  const head = z.year === now.year ? '' : `${z.year}/`
  return `${head}${z.month}/${z.day} ${time}`
}

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
  const kept = keptWarmAt ? Date.parse(keptWarmAt) : NaN
  const hotSince = Number.isFinite(kept) && kept > at ? kept : at
  const remainingSecs = Math.max(0, Math.min(ttlSecs, Math.round((hotSince + ttlSecs * 1000 - nowMs) / 1000)))
  const level = cacheLevel(remainingSecs, ttlSecs)
  const last = hotSince > at
    ? t('上次活動 {time}，已保溫 {warm}', { time: clock(new Date(at), nowMs), warm: clock(new Date(hotSince), nowMs) })
    : t('上次活動 {time}', { time: clock(new Date(at), nowMs) })
  const n = Math.max(1, Math.ceil(remainingSecs / 60))
  const title = level === 'cold'
    ? t('快取已涼（{last}）', { last })
    : t('快取約 {n} 分後到期（{last}）', { n, last })
  const frac = remainingSecs <= LOW_SECS ? Math.min(1, remainingSecs / LOW_SECS) : remainingSecs / ttlSecs
  return { level, remainingSecs, frac, title }
}

export const NO_WARM_TEXT = msg('不保溫：cache 到期前會自動壓縮（context 夠大時）')

export function chipCache(
  run: { last_api_at?: string | null; cache_ttl_secs?: number | null; cache_kept_warm_at?: string | null; keep_warm_skip?: boolean; cache_compacted?: boolean } | null | undefined,
  nowMs: number,
  working = false,
): CacheState | null {
  if (run?.keep_warm_skip === true) return null
  if (run?.cache_compacted === true && !working) return null
  return cacheState(run?.last_api_at, run?.cache_ttl_secs, nowMs, working, run?.cache_kept_warm_at)
}

export interface ApiChipCacheInfo {
  last_request_at?: string | null
  ttl_secs?: number | null
  expires_at?: string | null
  working?: boolean
}

export function apiChipCache(
  cache: ApiChipCacheInfo | null | undefined,
  nowMs: number,
  working = false,
): CacheState | null {
  if (!cache) return null
  const isWorking = working || cache.working === true
  const ttlSecs = cache.ttl_secs && cache.ttl_secs > 0 ? cache.ttl_secs : 300
  const at = cache.last_request_at ? Date.parse(cache.last_request_at) : NaN
  if (isWorking) {
    const when = Number.isFinite(at) ? t('（上次活動 {time}）', { time: clock(new Date(at), nowMs) }) : ''
    return { level: 'fresh', remainingSecs: ttlSecs, frac: 1, title: t('回合進行中，快取是熱的{when}', { when }) }
  }
  if (!cache.expires_at || !cache.ttl_secs || cache.ttl_secs <= 0) return null
  const exp = Date.parse(cache.expires_at)
  if (!Number.isFinite(exp)) return null
  const remainingSecs = Math.max(0, Math.min(cache.ttl_secs, Math.round((exp - nowMs) / 1000)))
  const level = cacheLevel(remainingSecs)
  const lastTime = Number.isFinite(at) ? clock(new Date(at), nowMs) : clock(new Date(exp - cache.ttl_secs * 1000), nowMs)
  const last = t('上次活動 {time}', { time: lastTime })
  if (level === 'cold') {
    return {
      level: 'cold',
      remainingSecs: 0,
      frac: 0,
      title: t('快取已涼（{last}）', { last }),
    }
  }
  const frac = remainingSecs <= LOW_SECS ? Math.min(1, remainingSecs / LOW_SECS) : remainingSecs / cache.ttl_secs
  const title = t('快取約 {n} 分後到期（{last}）', { n: Math.max(1, Math.ceil(remainingSecs / 60)), last })
  return { level, remainingSecs, frac, title }
}
