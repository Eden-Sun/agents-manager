import { useCallback } from 'react'

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
