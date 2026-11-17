import { useStore } from '../store/store'
import { intlLocale } from './i18n'
import { sameZonedDay as sameZonedDayOf, zonedDayStart as zonedDayStartOf, zonedParts as zonedPartsOf, type ZonedParts } from './timeZone'

/** 使用者設定的顯示時區（store；預設 Asia/Taipei，啟動時由 daemon 取一次）。 */
export function userTimeZone(): string {
  return useStore.getState().userTimeZone
}

type When = Date | number

/** 日期＋時間，一律用使用者時區。`opts` 同 `Intl.DateTimeFormatOptions`，但 `timeZone` 由這裡決定。 */
export function fmtDateTime(when: When, opts: Intl.DateTimeFormatOptions = {}): string {
  return new Date(when).toLocaleString(intlLocale(), { ...opts, timeZone: userTimeZone() })
}

/** 只顯示時間，一律用使用者時區。 */
export function fmtTime(when: When, opts: Intl.DateTimeFormatOptions = {}): string {
  return new Date(when).toLocaleTimeString(intlLocale(), { ...opts, timeZone: userTimeZone() })
}

/** 只顯示日期，一律用使用者時區。 */
export function fmtDate(when: When, opts: Intl.DateTimeFormatOptions = {}): string {
  return new Date(when).toLocaleDateString(intlLocale(), { ...opts, timeZone: userTimeZone() })
}

/** `when` 在使用者時區的牆上時間。 */
export function zonedParts(when: number | Date, zone: string = userTimeZone()): ZonedParts {
  return zonedPartsOf(when, zone)
}

/** 兩個時刻在使用者時區是不是同一天。 */
export function sameZonedDay(a: number | Date, b: number | Date, zone: string = userTimeZone()): boolean {
  return sameZonedDayOf(a, b, zone)
}

/** `when` 所在那天在使用者時區的 00:00（epoch 毫秒）。 */
export function zonedDayStart(when: number | Date, zone: string = userTimeZone()): number {
  return zonedDayStartOf(when, zone)
}

/** 元件用：時區一改就重畫（只為了當依賴，回傳值本身就是時區名稱）。 */
export function useUserTimeZone(): string {
  return useStore((s) => s.userTimeZone)
}
