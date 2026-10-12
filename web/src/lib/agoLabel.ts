/**
 * 「N 分鐘前／N 小時前／N 天前」（議題列表的更新時間）。不到一分鐘寫「剛剛」——以前 `Math.round` 會把 20 秒寫成「0 分鐘前」。
 * 未來時間（這台時鐘比 daemon 慢）也當「剛剛」，不寫負值。讀不懂回空字串。
 */
export function agoLabel(iso: string, now: number = Date.now()): string {
  const t = Date.parse(iso)
  if (Number.isNaN(t)) return ''
  const m = Math.max(0, Math.floor((now - t) / 60_000))
  if (m < 1) return '剛剛'
  if (m < 60) return `${m} 分鐘前`
  const h = Math.floor(m / 60)
  if (h < 48) return `${h} 小時前`
  return `${Math.floor(h / 24)} 天前`
}
