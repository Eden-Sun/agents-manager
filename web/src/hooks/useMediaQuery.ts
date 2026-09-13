import { useSyncExternalStore } from 'react'

/* 只有這兩個斷點改變行為（`docs/UI-DECISIONS.md`）；styles.css 其他斷點純樣式。 */

/** 須與 styles.css `@media (width <= 1024px)` 同值：以下側欄是離屏抽屜、關著要 inert。 */
export const DRAWER_QUERY = '(max-width: 1024px)'

/** 須與 styles.css `@media (width <= 640px)` 同值。 */
export const PHONE_QUERY = '(max-width: 640px)'

export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (onChange) => {
      const mq = window.matchMedia(query)
      mq.addEventListener('change', onChange)
      return () => mq.removeEventListener('change', onChange)
    },
    () => window.matchMedia(query).matches,
    // No DOM: assume desktop so the sidebar stays reachable to assistive tech.
    () => false,
  )
}
