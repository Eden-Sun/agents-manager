import { useEffect, useSyncExternalStore } from 'react'
import { nextLocalMidnight, startOfLocalDay } from '../lib/msgTime'
import { useUserTimeZone } from '../lib/userTime'

let today = startOfLocalDay(Date.now())
const listeners = new Set<() => void>()
let timer: ReturnType<typeof setTimeout> | null = null

function refresh() {
  const next = startOfLocalDay(Date.now())
  if (next !== today) {
    today = next
    for (const fn of listeners) fn()
  }
  arm()
}

function arm() {
  if (timer !== null) clearTimeout(timer)
  timer = null
  if (listeners.size === 0) return
  timer = setTimeout(refresh, Math.max(1000, nextLocalMidnight(Date.now()) - Date.now()))
}

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) refresh()
  })
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn)
  refresh()
  return () => {
    listeners.delete(fn)
    if (listeners.size === 0 && timer !== null) {
      clearTimeout(timer)
      timer = null
    }
  }
}

export function useLocalDay(): number {
  const zone = useUserTimeZone()
  useEffect(() => {
    refresh()
  }, [zone])
  return useSyncExternalStore(subscribe, () => today, () => today)
}
