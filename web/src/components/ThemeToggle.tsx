import { useState } from 'react'
import { applyTheme, loadTheme, type ThemeMode } from '../lib/theme'
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
