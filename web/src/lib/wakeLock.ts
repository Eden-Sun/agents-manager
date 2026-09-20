/**
 * 手機盯著畫面等 bot 回話時不要鎖屏（2026-09-20 使用者）。
 *
 * 用瀏覽器內建的 Screen Wake Lock：**只有安全來源（HTTPS 或 localhost）才有**——手機用
 * `http://<區網 IP>:7788` 開的話 `navigator.wakeLock` 是 undefined，這時要明說原因，不要靜靜失效。
 * 本機走 `https://<機器>.<tailnet>.ts.net:8443`（tailscale serve）就有。
 */
const KEY = 'am-keep-awake'

export type WakeSupport = 'ok' | 'insecure' | 'unsupported'

/** 這個瀏覽器／這個網址能不能用；`insecure` 是最常見的那種（http 開的頁面）。 */
export function wakeSupport(
  nav: { wakeLock?: unknown } = navigator,
  secure: boolean = window.isSecureContext,
): WakeSupport {
  if (nav.wakeLock) return 'ok'
  return secure ? 'unsupported' : 'insecure'
}

export function whyUnavailable(s: WakeSupport): string {
  if (s === 'insecure') return '這個網址不是 HTTPS，瀏覽器不給用（改用 https:// 的網址開就會出現）'
  return '這個瀏覽器沒有 Screen Wake Lock（iOS 要 16.4 以上）'
}

/** 開關存在這台裝置（手機與桌機各自決定）；存不了就只影響這一次。 */
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
    // 私密視窗存不了就只套這一次。
  }
}
