import { useSyncExternalStore } from 'react'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'

/**
 * 終端快照要不要折行。
 *
 * 桌機上終端是固定字元格線的東西，折行會把 TUI 畫的框線與對齊全部弄爛，所以預設不折、
 * 橫捲。手機不一樣：390px 大概只看得到 45 欄，一個 185 欄的 pane 每一行都被右邊裁掉，
 * 等於什麼都讀不到（`docs/goals/mobile-rwd-round2-2026-09-08.md` 問題 1）。所以預設值
 * 跟著 `PHONE_QUERY` 走，而使用者自己按過的選擇（localStorage）永遠優先。
 *
 * 三個面板（`TerminalTab` / `BlockedPanel` / `HostShellPanel`）共用同一個值，切一次就都跟著
 * 換——所以狀態放在 module scope 而不是各自的 useState。
 */
const KEY = 'am.term.wrap'

type Pref = 'wrap' | 'nowrap' | null

function read(): Pref {
  try {
    const v = localStorage.getItem(KEY)
    return v === 'wrap' || v === 'nowrap' ? v : null
  } catch {
    // Safari 無痕模式下讀 localStorage 會丟例外：當成「沒設過」，跟著斷點走就好。
    return null
  }
}

let pref: Pref = read()
const subs = new Set<() => void>()

function subscribe(fn: () => void): () => void {
  subs.add(fn)
  return () => {
    subs.delete(fn)
  }
}

/** 記下使用者的選擇並通知所有終端面板。 */
export function setTermWrap(on: boolean): void {
  pref = on ? 'wrap' : 'nowrap'
  try {
    localStorage.setItem(KEY, pref)
  } catch {
    // 記不起來就只在這次工作階段有效，不值得打斷使用者。
  }
  for (const fn of subs) fn()
}

/** 現在該不該折行：使用者設過就聽他的，沒設過就手機折、桌機不折。 */
export function useTermWrap(): boolean {
  const p = useSyncExternalStore(
    subscribe,
    () => pref,
    () => null,
  )
  const phone = useMediaQuery(PHONE_QUERY)
  return p === null ? phone : p === 'wrap'
}
