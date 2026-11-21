export function limitHitWhen(until: string | null | undefined, now: number = Date.now()): string | null {
  if (!until) return null
  const d = new Date(until)
  if (Number.isNaN(d.getTime())) return null
  const p = (n: number) => String(n).padStart(2, '0')
  const zone = userTimeZone()
  const z = zonedParts(d, zone)
  const time = `${p(z.hour)}:${p(z.minute)}`
  if (sameZonedDay(d, now, zone)) return time
  const head = z.year === zonedParts(now, zone).year ? '' : `${z.year}/`
  return `${head}${z.month}/${z.day} ${time}`
}

import { sameZonedDay, userTimeZone, zonedParts } from './userTime'