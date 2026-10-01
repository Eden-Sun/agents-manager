import { useStore } from '../store/store'
import { grokUpdatePlan, type GrokUpdatePlan } from '../lib/grokUpdate'

/** `grokUpdatePlan` 的 store 版：selector 回字串（回新物件會 getSnapshot should be cached）。 */
export function useGrokPlan(): GrokUpdatePlan | null {
  const key = useStore((s) => {
    const p = grokUpdatePlan(s.upstreamUpdates.grok)
    return p ? JSON.stringify(p) : ''
  })
  return key ? (JSON.parse(key) as GrokUpdatePlan) : null
}
