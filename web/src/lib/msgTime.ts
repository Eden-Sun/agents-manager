import { userTimeZone, zonedDayStart, zonedParts } from './userTime'

const p = (n: number) => String(n).padStart(2, '0')

/** 今天只寫時分，不是今天帶月日，不同年再帶年。讀不懂回空字串。`now` 只看日期部分，傳當天任一時刻或當天的開始都可以。（時區：使用者設定，#2047） */
export function msgTimeLabel(iso: string, now: number = Date.now(), zone: string = userTimeZone()): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  const a = zonedParts(d, zone)
  const b = zonedParts(now, zone)
  const time = `${p(a.hour)}:${p(a.minute)}`
  if (a.year !== b.year) return `${a.year}/${a.month}/${a.day} ${time}`
  if (a.month !== b.month || a.day !== b.day) return `${a.month}/${a.day} ${time}`
  return time
}

/** 使用者時區「今天」的開始（0 點）。跨日時它會變，拿來當 [`msgTimeLabel`] 的 `now`，標籤才會跟著換天。 */
export function startOfLocalDay(now: number, zone: string = userTimeZone()): number {
  return zonedDayStart(now, zone)
}

/** 下一個使用者時區的午夜（嚴格在 `now` 之後）。今天開始 +25 小時一定落在明天（夏令時間的日子也是）。 */
export function nextLocalMidnight(now: number, zone: string = userTimeZone()): number {
  return zonedDayStart(zonedDayStart(now, zone) + 25 * 3_600_000, zone)
}

/** tooltip 的完整時間，使用者時區 `YYYY-MM-DD HH:mm:ss`；讀不懂就原樣顯示。 */
export function msgTimeTitle(iso: string, zone: string = userTimeZone()): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const a = zonedParts(d, zone)
  return `${a.year}-${p(a.month)}-${p(a.day)} ${p(a.hour)}:${p(a.minute)}:${p(a.second)}`
}
