import { useCallback, useSyncExternalStore } from 'react'

export const DRAWER_QUERY = '(max-width: 1024px)'

export const PHONE_QUERY = '(max-width: 640px)'

export function useMediaQuery(query: string): boolean {
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
    () => false,
  )
}
