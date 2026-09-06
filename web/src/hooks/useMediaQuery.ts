import { useSyncExternalStore } from 'react'

/**
 * The one place the sidebar's drawer breakpoint is written down for JS. It must stay in step
 * with the `@media (width <= 1024px)` block in styles.css that turns the sidebar into an
 * off-canvas drawer: below it the closed sidebar is offscreen and must be inert, above it the
 * sidebar is an ordinary always-visible column and must stay reachable.
 */
export const MOBILE_QUERY = '(max-width: 1024px)'

/** `true` while the media query matches, re-rendering when that flips. */
export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (onChange) => {
      const mq = window.matchMedia(query)
      mq.addEventListener('change', onChange)
      return () => mq.removeEventListener('change', onChange)
    },
    () => window.matchMedia(query).matches,
    // No DOM during SSR/prerender: assume the desktop layout, which leaves the sidebar
    // reachable rather than hiding it from assistive tech.
    () => false,
  )
}
