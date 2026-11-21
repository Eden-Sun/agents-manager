import { useEffect, useMemo, useState } from 'react'
import { botOfflineHost, offlineHosts, type OfflineHost } from '../lib/hostOffline'
import { useStore } from '../store/store'

export function useNow(on: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!on) return
    const tick = () => setNow(Date.now())
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

export function useBotOfflineHost(botId: string | null): string | null {
  return useStore((s) => botOfflineHost(s, botId))
}
