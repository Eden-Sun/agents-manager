import { useEffect, type Dispatch, type SetStateAction } from 'react'
import { BOT_TOP_EVENT, finishBotToTop, scrollBotRowToTop } from '../lib/scrollBot'
import { persistSetDiff } from '../store/sharedSet'
import { useStore } from '../store/store'

export function useBotScrollTop(
  setShutProjects: Dispatch<SetStateAction<Set<string>>>,
  setCollapsed: Dispatch<SetStateAction<Set<string>>>,
): void {
  useEffect(() => {
    let raf = 0
    const onRequest = (e: Event) => {
      const botId = (e as CustomEvent<{ botId: string }>).detail?.botId
      if (!botId) return
      const bots = useStore.getState().bots
      const me = bots.find((b) => b.id === botId)
      if (!me) {
        finishBotToTop(botId)
        return
      }
      const ancestors: string[] = []
      for (let p = me.parent_bot_id, i = 0; p && i < bots.length; i += 1) {
        ancestors.push(p)
        p = bots.find((x) => x.id === p)?.parent_bot_id ?? null
      }
      const open = (key: string, drop: string[]) => (prev: Set<string>) => {
        if (!drop.some((d) => prev.has(d))) return prev
        const next = new Set(prev)
        for (const d of drop) next.delete(d)
        try {
          persistSetDiff(key, prev, next)
        } catch {
        }
        return next
      }
      setShutProjects(open('am.collapsedProjects', [me.project_id]))
      setCollapsed(open('am.collapsedChildren', ancestors))
      cancelAnimationFrame(raf)
      raf = requestAnimationFrame(() => {
        scrollBotRowToTop(botId)
        finishBotToTop(botId)
      })
    }
    window.addEventListener(BOT_TOP_EVENT, onRequest)
    return () => {
      window.removeEventListener(BOT_TOP_EVENT, onRequest)
      cancelAnimationFrame(raf)
    }
  }, [setShutProjects, setCollapsed])
}
