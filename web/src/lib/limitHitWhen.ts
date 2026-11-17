/** 被擋到什麼時候：今天只寫時分；不是今天帶月日，跨年再帶年。讀不懂回 null。 */
export function limitHitWhen(until: string | null | undefined, now: number = Date.now()): string | null {
  if (!until) return null
  const d = new Date(until)
  if (Number.isNaN(d.getTime())) return null
  const p = (n: number) => String(n).padStart(2, '0')
  const zone = userTimeZone()
  const z = zonedParts(d, zone)
  const time = `${p(z.hour)}:${p(z.minute)}`
  if (sameZonedDay(d, now, zone)) return time
  // 跨年要帶年份：1 月的 `1/1 09:00` 會被讀成今年已過的日子（#1344 之後的重置徽章）。
  const head = z.year === zonedParts(now, zone).year ? '' : `${z.year}/`
  return `${head}${z.month}/${z.day} ${time}`
}

import { sameZonedDay, userTimeZone, zonedParts } from './userTime'