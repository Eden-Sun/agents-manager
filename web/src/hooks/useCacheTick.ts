import { useEffect, useState } from 'react'
import { CACHE_TICK_MS } from '../lib/cacheClock'

/** 快取倒數用的「現在」：`active` 時每 15 秒換一次值觸發重算（SPEC §6.5j）；不存任何東西。 */
export function useCacheTick(active: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const id = window.setInterval(() => setNow(Date.now()), CACHE_TICK_MS)
    return () => window.clearInterval(id)
  }, [active])
  return now
}
