import { projectHostName, useStore } from '../store/store'
import { herdrUpdatePlan, type HerdrUpdatePlan } from '../store/herdrUpdate'

/** `herdrUpdatePlan` 的 store 版：selector 回字串（回新物件會 getSnapshot should be cached）。 */
export function useHerdrPlan(): HerdrUpdatePlan | null {
  const key = useStore((s) => {
    const shared = new Set(s.hosts.filter((h) => h.shared_session).map((h) => h.name))
    const p = herdrUpdatePlan(s.upstreamUpdates.herdr, shared, s.bots, s.runs, (b) => projectHostName(s, b.project_id))
    return p ? JSON.stringify(p) : ''
  })
  return key ? (JSON.parse(key) as HerdrUpdatePlan) : null
}
