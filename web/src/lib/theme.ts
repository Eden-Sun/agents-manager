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
  }
}

export function themeFromStorageEvent(key: string | null, newValue: string | null, root: ThemeRoot = document.documentElement): ThemeMode | null {
  if (key !== null && key !== KEY) return null
  const mode: ThemeMode = newValue === 'light' || newValue === 'dark' ? newValue : 'system'
  paint(root, mode)
  return mode
}
