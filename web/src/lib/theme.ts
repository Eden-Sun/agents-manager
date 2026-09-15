/** 深淺主題：跟隨系統／淺／深（2026-09-15 使用者）。存在這台瀏覽器，html 上蓋 data-theme，CSS 見 styles.css。 */
export type ThemeMode = 'system' | 'light' | 'dark'

const KEY = 'am-theme'

export function loadTheme(): ThemeMode {
  try {
    const v = localStorage.getItem(KEY)
    return v === 'light' || v === 'dark' ? v : 'system'
  } catch {
    return 'system'
  }
}

export function applyTheme(mode: ThemeMode): void {
  const root = document.documentElement
  if (mode === 'system') root.removeAttribute('data-theme')
  else root.setAttribute('data-theme', mode)
  try {
    if (mode === 'system') localStorage.removeItem(KEY)
    else localStorage.setItem(KEY, mode)
  } catch {
    // 私密視窗存不了就只套這一次。
  }
}
