import { useCallback, useSyncExternalStore } from 'react'

/* 只有這兩個斷點改變行為（`docs/UI-DECISIONS.md`）；styles.css 其他斷點純樣式。 */

/** 須與 styles.css `@media (width <= 1024px)` 同值：以下側欄是離屏抽屜、關著要 inert。 */
export const DRAWER_QUERY = '(max-width: 1024px)'

/** 須與 styles.css `@media (width <= 640px)` 同值。 */
export const PHONE_QUERY = '(max-width: 640px)'

export function useMediaQuery(query: string): boolean {
  // subscribe 要跟著 query 穩定：行內箭頭函式每次 render 身分都變，React 會 unsubscribe 再 subscribe（#1643）。
  const subscribe = useCallback(
    (onChange: () => void) => {
      const mq = window.matchMedia(query)
      mq.addEventListener('change', onChange)
      return () => mq.removeEventListener('change', onChange)
    },
    [query],
  )
  const snapshot = useCallback(() => window.matchMedia(query).matches, [query])
  return useSyncExternalStore(
    subscribe,
    snapshot,
    // No DOM: assume desktop so the sidebar stays reachable to assistive tech.
    () => false,
  )
}
