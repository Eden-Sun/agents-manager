import { useStore } from '../store/store'
import { grokUpdatePlan, type GrokUpdatePlan } from '../lib/grokUpdate'

export function useGrokPlan(): GrokUpdatePlan | null {
  const key = useStore((s) => {
    const p = grokUpdatePlan(s.upstreamUpdates.grok)
    return p ? JSON.stringify(p) : ''
  })
  return key ? (JSON.parse(key) as GrokUpdatePlan) : null
}
