import { useCallback } from 'react'

/**
 * React 17+ 的 `onWheel` 是 passive，`preventDefault` 無效，所以改掛原生非 passive listener
 * （docs/reviews/2026-09-12/web.md §4 passive wheel）。回 ref callback，可掛多個節點。
 */
export function useWheelRef<T extends HTMLElement>(handler: (e: WheelEvent) => void): (el: T | null) => void | (() => void) {
  return useCallback(
    (el: T | null) => {
      if (!el) return
      el.addEventListener('wheel', handler, { passive: false })
      return () => el.removeEventListener('wheel', handler)
    },
    [handler],
  )
}
