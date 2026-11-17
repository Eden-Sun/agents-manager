/** 使用者時區的純函式（不碰 store，給 mock 與 helper 共用）。 */
export const DEFAULT_USER_TIME_ZONE = 'Asia/Taipei'

/** 瀏覽器認不認得這個 IANA 時區名稱。 */
export function isValidTimeZone(zone: string): boolean {
  if (!zone) return false
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: zone })
    return true
  } catch {
    return false
  }
}

/** 某時區的 UTC 偏移，daemon 同款寫法：`+8`、`-5`、`+5:30`、`+0`；認不得回空字串。 */
export function utcOffsetLabel(zone: string, at: number = Date.now()): string {
  try {
    const name = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'longOffset' }).formatToParts(at).find((p) => p.type === 'timeZoneName')?.value ?? ''
    const m = /^GMT(?:([+-])(\d{1,2})(?::(\d{2}))?)?$/.exec(name)
    if (!m) return ''
    if (!m[1]) return '+0'
    const minutes = m[3] && m[3] !== '00' ? `:${m[3]}` : ''
    return `${m[1]}${Number(m[2])}${minutes}`
  } catch {
    return ''
  }
}
