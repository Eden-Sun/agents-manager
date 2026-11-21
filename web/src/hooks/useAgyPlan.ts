import { useStore } from '../store/store'
import { agyUpdatePlan, type AgyUpdatePlan } from '../lib/agyUpdate'

export function useAgyBusy(): boolean {
  return useStore((s) => {
    const hosts = s.upstreamUpdates.agy?.hosts ?? []
    return hosts.some((h) => h.behind && s.busy[`agy-install:${h.host || 'local'}`] === true)
  })
}

export function useAgyPlan(): AgyUpdatePlan | null {
  const key = useStore((s) => {
    const p = agyUpdatePlan(s.upstreamUpdates.agy)
    return p ? JSON.stringify(p) : ''
  })
  return key ? (JSON.parse(key) as AgyUpdatePlan) : null
}
