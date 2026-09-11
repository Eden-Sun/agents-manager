import type { KeyboardEvent } from 'react'

/**
 * `role="tablist"` 的方向鍵（WAI-ARIA APG tabs，自動啟用）：←/→ 換到隔壁那個分頁、
 * Home/End 跳頭尾，焦點與選取一起走（直接 click 它，切換邏輯只寫在按鈕的 onClick 一處）。
 *
 * 分頁本身**不做** roving tabindex，每個分頁照樣是一個 Tab 停點：UI-DECISIONS 的
 * 「只用 Tab / Enter / Space / Escape 就能切聊天／終端」是驗收條件，方向鍵是額外加的路。
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
