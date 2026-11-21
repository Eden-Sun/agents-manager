import { userTimeZone, zonedDayStart, zonedParts } from './userTime'

const p = (n: number) => String(n).padStart(2, '0')

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

export function startOfLocalDay(now: number, zone: string = userTimeZone()): number {
  return zonedDayStart(now, zone)
}

export function nextLocalMidnight(now: number, zone: string = userTimeZone()): number {
  return zonedDayStart(zonedDayStart(now, zone) + 25 * 3_600_000, zone)
}

export function msgTimeTitle(iso: string, zone: string = userTimeZone()): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const a = zonedParts(d, zone)
  return `${a.year}-${p(a.month)}-${p(a.day)} ${p(a.hour)}:${p(a.minute)}:${p(a.second)}`
}
