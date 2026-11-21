import { useEffect, useState } from 'react'
import { CACHE_RED_TICK_MS, CACHE_TICK_MS } from '../lib/cacheClock'

export function useCacheTick(active: boolean, red = false): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const tick = () => setNow(Date.now())
    const first = window.setTimeout(tick, 0)
    const id = window.setInterval(tick, red ? CACHE_RED_TICK_MS : CACHE_TICK_MS)
    return () => {
      window.clearTimeout(first)
      window.clearInterval(id)
    }
  }, [active, red])
  return now
}
