import { useSyncExternalStore } from 'react'

const KEY = 'am.term.wrap'

type Pref = 'wrap' | 'nowrap' | null

function read(): Pref {
  try {
    const v = localStorage.getItem(KEY)
    return v === 'wrap' || v === 'nowrap' ? v : null
  } catch {
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
  }
  for (const fn of subs) fn()
}

export function useTermWrap(): boolean {
  const p = useSyncExternalStore(
    subscribe,
    () => pref,
    () => null,
  )
  return p !== 'nowrap'
}
