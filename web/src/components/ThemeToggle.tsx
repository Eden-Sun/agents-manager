import { useState } from 'react'
import { applyTheme, loadTheme, type ThemeMode } from '../lib/theme'
import './themeToggle.css'

const MODES: { mode: ThemeMode; label: string; title: string }[] = [
  { mode: 'system', label: '自動', title: '跟隨系統的深淺設定' },
  { mode: 'light', label: '淺', title: '固定淺色' },
  { mode: 'dark', label: '深', title: '固定深色' },
]

/** 左上角的三段式主題切換（2026-09-15 使用者）。 */
export function ThemeToggle() {
  const [mode, setMode] = useState<ThemeMode>(loadTheme)
  return (
    <span className="theme-toggle" role="radiogroup" aria-label="主題">
      {MODES.map((m) => (
        <button
          key={m.mode}
          type="button"
          role="radio"
          aria-checked={mode === m.mode}
          className={mode === m.mode ? 'on' : undefined}
          title={m.title}
          onClick={() => {
            applyTheme(m.mode)
            setMode(m.mode)
          }}
        >
          {m.label}
        </button>
      ))}
    </span>
  )
}
