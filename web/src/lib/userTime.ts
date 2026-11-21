import { useStore } from '../store/store'
import { intlLocale } from './i18n'
import { sameZonedDay as sameZonedDayOf, zonedDayStart as zonedDayStartOf, zonedParts as zonedPartsOf, type ZonedParts } from './timeZone'

export function userTimeZone(): string {
  return useStore.getState().userTimeZone
}

type When = Date | number

export function fmtDateTime(when: When, opts: Intl.DateTimeFormatOptions = {}): string {
  return new Date(when).toLocaleString(intlLocale(), { ...opts, timeZone: userTimeZone() })
}

export function fmtTime(when: When, opts: Intl.DateTimeFormatOptions = {}): string {
  return new Date(when).toLocaleTimeString(intlLocale(), { ...opts, timeZone: userTimeZone() })
}

export function fmtDate(when: When, opts: Intl.DateTimeFormatOptions = {}): string {
  return new Date(when).toLocaleDateString(intlLocale(), { ...opts, timeZone: userTimeZone() })
}

export function zonedParts(when: number | Date, zone: string = userTimeZone()): ZonedParts {
  return zonedPartsOf(when, zone)
}

export function sameZonedDay(a: number | Date, b: number | Date, zone: string = userTimeZone()): boolean {
  return sameZonedDayOf(a, b, zone)
}

export function zonedDayStart(when: number | Date, zone: string = userTimeZone()): number {
  return zonedDayStartOf(when, zone)
}

export function useUserTimeZone(): string {
  return useStore((s) => s.userTimeZone)
}
