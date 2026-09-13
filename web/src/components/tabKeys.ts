import type { KeyboardEvent } from 'react'

/**
 * `role="tablist"` 的方向鍵（WAI-ARIA APG tabs，自動啟用），透過 click 切換。
 * 不做 roving tabindex：UI-DECISIONS 的驗收條件是只用 Tab 就能切聊天／終端。
 */
export function onTabListKeyDown(e: KeyboardEvent<HTMLElement>) {
  const step = { ArrowRight: 1, ArrowLeft: -1 }[e.key]
  if (step === undefined && e.key !== 'Home' && e.key !== 'End') return
  const tabs = [...e.currentTarget.querySelectorAll<HTMLButtonElement>('[role="tab"]')].filter((t) => !t.disabled)
  const at = tabs.indexOf(document.activeElement as HTMLButtonElement)
  if (at < 0) return
  e.preventDefault()
  const next = e.key === 'Home' ? 0 : e.key === 'End' ? tabs.length - 1 : (at + step! + tabs.length) % tabs.length
  tabs[next].focus()
  if (next !== at) tabs[next].click()
}
