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

/** 某時區在某一刻的牆上時間（年、月 1–12、日、時 0–23、分、秒）。 */
export interface ZonedParts {
  year: number
  month: number
  day: number
  hour: number
  minute: number
  second: number
}

const partFormats = new Map<string, Intl.DateTimeFormat>()

function partFormat(zone: string): Intl.DateTimeFormat {
  let f = partFormats.get(zone)
  if (!f) {
    try {
      f = new Intl.DateTimeFormat('en-US', { timeZone: zone, hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric', second: 'numeric' })
    } catch {
      // 認不得的時區名稱退回 UTC，不讓畫面整個炸掉。
      f = new Intl.DateTimeFormat('en-US', { timeZone: 'UTC', hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric', second: 'numeric' })
    }
    partFormats.set(zone, f)
  }
  return f
}

/** `when`（Date 或毫秒）在 `zone` 的牆上時間。 */
export function zonedParts(when: number | Date, zone: string): ZonedParts {
  const ms = typeof when === 'number' ? when : when.getTime()
  const o: Record<string, number> = {}
  for (const p of partFormat(zone).formatToParts(ms)) if (p.type !== 'literal') o[p.type] = Number(p.value)
  return { year: o.year, month: o.month, day: o.day, hour: o.hour % 24, minute: o.minute, second: o.second }
}

/** `ms` 那一刻，`zone` 比 UTC 快多少毫秒。 */
function offsetAt(ms: number, zone: string): number {
  const p = zonedParts(ms, zone)
  return Date.UTC(p.year, p.month - 1, p.day, p.hour, p.minute, p.second) - Math.floor(ms / 1000) * 1000
}

/** 兩個時刻在 `zone` 是不是同一天。 */
export function sameZonedDay(a: number | Date, b: number | Date, zone: string): boolean {
  const x = zonedParts(a, zone)
  const y = zonedParts(b, zone)
  return x.year === y.year && x.month === y.month && x.day === y.day
}

/** `when` 所在那天在 `zone` 的 00:00，epoch 毫秒（夏令時間切換的那天也算對）。 */
export function zonedDayStart(when: number | Date, zone: string): number {
  const ms = typeof when === 'number' ? when : when.getTime()
  const p = zonedParts(ms, zone)
  const wall = Date.UTC(p.year, p.month - 1, p.day)
  const first = offsetAt(ms, zone)
  let cand = wall - first
  const again = offsetAt(cand, zone)
  if (again !== first) cand = wall - again
  return cand
}
