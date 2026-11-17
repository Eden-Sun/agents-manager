import { useStore } from '../store/store'
import { intlLocale } from './i18n'

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
