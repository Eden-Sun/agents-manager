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

type ThemeRoot = Pick<HTMLElement, 'setAttribute' | 'removeAttribute'>

function paint(root: ThemeRoot, mode: ThemeMode): void {
  if (mode === 'system') root.removeAttribute('data-theme')
  else root.setAttribute('data-theme', mode)
}

export function applyTheme(mode: ThemeMode): void {
  paint(document.documentElement, mode)
  try {
    if (mode === 'system') localStorage.removeItem(KEY)
    else localStorage.setItem(KEY, mode)
  } catch {
    // 私密視窗存不了就只套這一次。
  }
}

/**
 * 另一個分頁改了主題（`storage` 事件只送給其他分頁）：照那個值套到這個分頁，不寫回儲存。
 * 不是主題的鍵回 `null`；是的話回新的模式（左上角主題鈕的狀態跟著它走）。
 */
export function themeFromStorageEvent(key: string | null, newValue: string | null, root: ThemeRoot = document.documentElement): ThemeMode | null {
  if (key !== null && key !== KEY) return null
  const mode: ThemeMode = newValue === 'light' || newValue === 'dark' ? newValue : 'system'
  paint(root, mode)
  return mode
}
