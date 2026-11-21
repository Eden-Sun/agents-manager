export const DEFAULT_USER_TIME_ZONE = 'Asia/Taipei'

export function isValidTimeZone(zone: string): boolean {
  if (!zone) return false
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: zone })
    return true
  } catch {
    return false
  }
}

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
      f = new Intl.DateTimeFormat('en-US', { timeZone: 'UTC', hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric', second: 'numeric' })
    }
    partFormats.set(zone, f)
  }
  return f
}

export function zonedParts(when: number | Date, zone: string): ZonedParts {
  const ms = typeof when === 'number' ? when : when.getTime()
  const o: Record<string, number> = {}
  for (const p of partFormat(zone).formatToParts(ms)) if (p.type !== 'literal') o[p.type] = Number(p.value)
  return { year: o.year, month: o.month, day: o.day, hour: o.hour % 24, minute: o.minute, second: o.second }
}

function offsetAt(ms: number, zone: string): number {
  const p = zonedParts(ms, zone)
  return Date.UTC(p.year, p.month - 1, p.day, p.hour, p.minute, p.second) - Math.floor(ms / 1000) * 1000
}

export function sameZonedDay(a: number | Date, b: number | Date, zone: string): boolean {
  const x = zonedParts(a, zone)
  const y = zonedParts(b, zone)
  return x.year === y.year && x.month === y.month && x.day === y.day
}

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
