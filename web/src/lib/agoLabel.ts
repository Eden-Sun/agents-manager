import { t } from './i18n'

/**
 * 「N 分鐘前／N 小時前／N 天前」（議題列表的更新時間）。不到一分鐘寫「剛剛」——以前 `Math.round` 會把 20 秒寫成「0 分鐘前」。
 * 未來時間（這台時鐘比 daemon 慢）也當「剛剛」，不寫負值。讀不懂回空字串。
 */
export function agoLabel(iso: string, now: number = Date.now()): string {
  const timestamp = Date.parse(iso)
  if (Number.isNaN(timestamp)) return ''
  const m = Math.max(0, Math.floor((now - timestamp) / 60_000))
  if (m < 1) return t('剛剛')
  if (m < 60) return t('{n} 分鐘前', { n: m })
  const h = Math.floor(m / 60)
  if (h < 48) return t('{n} 小時前', { n: h })
  return t('{n} 天前', { n: Math.floor(h / 24) })
}
