import { t } from './i18n'

const KEY = 'am-keep-awake'

export type WakeSupport = 'ok' | 'insecure' | 'unsupported'

export function wakeSupport(
  nav: { wakeLock?: unknown } = navigator,
  secure: boolean = window.isSecureContext,
): WakeSupport {
  if (nav.wakeLock) return 'ok'
  return secure ? 'unsupported' : 'insecure'
}

export function whyUnavailable(s: WakeSupport): string {
  if (s === 'insecure') return t('這個網址不是 HTTPS，瀏覽器不給用（改用 https:// 的網址開就會出現）')
  return t('這個瀏覽器沒有 Screen Wake Lock（iOS 要 16.4 以上）')
}

export function loadKeepAwake(): boolean {
  try {
    return localStorage.getItem(KEY) === '1'
  } catch {
    return false
  }
}

export function saveKeepAwake(on: boolean): void {
  try {
    if (on) localStorage.setItem(KEY, '1')
    else localStorage.removeItem(KEY)
  } catch {
  }
}
