import { useEffect, useState } from 'react'
import { CACHE_TICK_MS } from '../lib/cacheClock'

/** 快取倒數用的「現在」：`active` 時每 15 秒換一次值觸發重算（SPEC §6.5j）；不存任何東西。 */
export function useCacheTick(active: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const tick = () => setNow(Date.now())
    // 剛變成 active 時先補一次：掛載時拿的時間可能是很久以前（同 useHostOffline 的 useNow）。
    const first = window.setTimeout(tick, 0)
    const id = window.setInterval(tick, CACHE_TICK_MS)
    return () => {
      window.clearTimeout(first)
      window.clearInterval(id)
    }
  }, [active])
  return now
}
