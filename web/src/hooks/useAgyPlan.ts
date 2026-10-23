import { useStore } from '../store/store'
import { agyUpdatePlan, type AgyUpdatePlan } from '../lib/agyUpdate'

/** 有任何一台落後的主機正在裝 agy（chip 停用、手機選單那一項也停用）。布林值，selector 穩定。 */
export function useAgyBusy(): boolean {
  return useStore((s) => {
    const hosts = s.upstreamUpdates.agy?.hosts ?? []
    return hosts.some((h) => h.behind && s.busy[`agy-install:${h.host || 'local'}`] === true)
  })
}

/** `agyUpdatePlan` 的 store 版：selector 回字串（回新物件會 getSnapshot should be cached）。 */
export function useAgyPlan(): AgyUpdatePlan | null {
  const key = useStore((s) => {
    const p = agyUpdatePlan(s.upstreamUpdates.agy)
    return p ? JSON.stringify(p) : ''
  })
  return key ? (JSON.parse(key) as AgyUpdatePlan) : null
}
