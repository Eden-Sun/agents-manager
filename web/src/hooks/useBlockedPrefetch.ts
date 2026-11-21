import { useEffect } from 'react'
import { forgetBlocked, prefetchBlocked, PREFETCH_LINES, PREFETCH_SOURCE, type PrefetchDeps } from '../lib/blockedPrefetch'
import { parseChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'

const REFRESH_MS = 4_000

function deps(): PrefetchDeps {
  const s = () => useStore.getState()
  return {
    read: (botId) => s().readTerminal(botId, PREFETCH_SOURCE, PREFETCH_LINES),
    io: (botId) => ({
      read: async () => {
        try {
          return parseChoiceMenu((await s().readTerminal(botId, 'visible', 60)).text)
        } catch {
          return null
        }
      },
      send: (keys) => s().sendKeys(botId, keys),
      wait: (ms) => new Promise((r) => setTimeout(r, ms)),
      paste: async (text) => {
        await s().sendText(botId, text, false)
      },
    }),
    stillBlocked: (id) => useStore.getState().runs[id]?.agent_status === 'blocked',
    visible: () => typeof document === 'undefined' || document.visibilityState === 'visible',
  }
}

function blockedIds(s: ReturnType<typeof useStore.getState>): string {
  return Object.entries(s.runs)
    .filter(([, r]) => r?.agent_status === 'blocked')
    .map(([id]) => id)
    .sort()
    .join(' ')
}

export function useBlockedPrefetch(): void {
  const ids = useStore(blockedIds)
  useEffect(() => {
    const list = ids ? ids.split(' ') : []
    if (!list.length) return
    const d = deps()
    let alive = true
    const tick = () => {
      if (!alive || document.visibilityState === 'hidden') return
      for (const id of list) void prefetchBlocked(id, d)
    }
    const onVisible = () => {
      if (document.visibilityState === 'visible') tick()
    }
    document.addEventListener('visibilitychange', onVisible)
    tick()
    const t = setInterval(tick, REFRESH_MS)
    return () => {
      alive = false
      document.removeEventListener('visibilitychange', onVisible)
      clearInterval(t)
      const still = new Set(blockedIds(useStore.getState()).split(' '))
      for (const id of list) if (!still.has(id)) forgetBlocked(id)
    }
  }, [ids])
}
