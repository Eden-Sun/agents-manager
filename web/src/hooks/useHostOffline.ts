import { useEffect, useMemo, useState } from 'react'
import { botOfflineHost, offlineHosts, type OfflineHost } from '../lib/hostOffline'
import { useStore } from '../store/store'

/** 「離線多久」每 30 秒重算一次；沒有離線主機時不起計時器。 */
export function useNow(on: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!on) return
    const tick = () => setNow(Date.now())
    // 剛變成離線時先補一次：掛載時拿的時間可能是很久以前。
    const first = setTimeout(tick, 0)
    const id = setInterval(tick, 30_000)
    return () => {
      clearTimeout(first)
      clearInterval(id)
    }
  }, [on])
  return now
}

export function useOfflineHosts(): OfflineHost[] {
  const hosts = useStore((s) => s.hosts)
  const projects = useStore((s) => s.projects)
  const bots = useStore((s) => s.bots)
  return useMemo(() => offlineHosts({ hosts, projects, bots }), [hosts, projects, bots])
}

/** 這顆 bot 所在的離線遠端主機名；本機或連著回 null。 */
export function useBotOfflineHost(botId: string | null): string | null {
  return useStore((s) => botOfflineHost(s, botId))
}
