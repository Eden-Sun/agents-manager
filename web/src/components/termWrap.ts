import { useSyncExternalStore } from 'react'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'

/**
 * 終端快照要不要折行：桌機不折（會弄爛 TUI 框線），手機折（mobile-rwd-round2-2026-09-08 問題 1）；使用者選擇優先。
 * 三個終端面板共用，所以狀態放 module scope。
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
