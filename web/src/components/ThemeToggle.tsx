import { useEffect, useState } from 'react'
import { applyTheme, loadTheme, themeFromStorageEvent, type ThemeMode } from '../lib/theme'
import './themeToggle.css'

const NEXT: Record<ThemeMode, ThemeMode> = { system: 'light', light: 'dark', dark: 'system' }
const LOOK: Record<ThemeMode, { icon: string; name: string }> = {
  system: { icon: '◐', name: '跟隨系統' },
  light: { icon: '☀', name: '淺色' },
  dark: { icon: '☾', name: '深色' },
}

/** 左上角主題鈕：同一顆按鈕三段輪流切換 自動 → 淺 → 深（2026-09-15 使用者）。 */
export function ThemeToggle() {
  const [mode, setMode] = useState<ThemeMode>(loadTheme)
  const next = NEXT[mode]
  // 另一個分頁換了主題：這顆鈕的圖示跟著變（DOM 本身由 main.tsx 的監聽套用）。
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      const m = themeFromStorageEvent(e.key, e.newValue)
      if (m) setMode(m)
    }
    window.addEventListener('storage', onStorage)
    return () => window.removeEventListener('storage', onStorage)
  }, [])
  return (
    <button
      type="button"
      className="theme-toggle"
      title={`主題：${LOOK[mode].name}（點一下換成${LOOK[next].name}）`}
      aria-label={`主題：${LOOK[mode].name}`}
      onClick={() => {
        applyTheme(next)
        setMode(next)
      }}
    >
      {LOOK[mode].icon}
    </button>
  )
}
