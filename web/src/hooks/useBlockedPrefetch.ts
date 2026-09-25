import { useEffect } from 'react'
import { forgetBlocked, prefetchBlocked, PREFETCH_LINES, PREFETCH_SOURCE, type PrefetchDeps } from '../lib/blockedPrefetch'
import { parseChoiceMenu } from '../lib/tuiChoices'
import { useStore } from '../store/store'

/** blocked 期間多久重讀一次快取（視窗晚點才打開也拿得到新鮮的畫面；見 `PREFETCH_FRESH_MS`）。 */
const REFRESH_MS = 4_000

function deps(): PrefetchDeps {
  const s = () => useStore.getState()
  return {
    read: (botId) => s().readTerminal(botId, PREFETCH_SOURCE, PREFETCH_LINES),
    // 與 `BlockedDraft` 的 io 同一套：讀 60 行就夠，選單只在畫面下半部。
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
    visible: () => typeof document === 'undefined' || document.visibilityState === 'visible',
  }
}

/** 哪些 bot 現在 blocked（純字串，selector 回陣列會每次都是新參考）。 */
function blockedIds(s: ReturnType<typeof useStore.getState>): string {
  return Object.entries(s.runs)
    .filter(([, r]) => r?.agent_status === 'blocked')
    .map(([id]) => id)
    .sort()
    .join(' ')
}

/** 掛在 App 一次：bot 一進 blocked 就在背景讀好問卷（`lib/blockedPrefetch.ts`）。 */
export function useBlockedPrefetch(): void {
  const ids = useStore(blockedIds)
  useEffect(() => {
    const list = ids ? ids.split(' ') : []
    if (!list.length) return
    const d = deps()
    let alive = true
    const tick = () => {
      if (!alive) return
      for (const id of list) void prefetchBlocked(id, d)
    }
    tick()
    const t = setInterval(tick, REFRESH_MS)
    return () => {
      alive = false
      clearInterval(t)
      const still = new Set(blockedIds(useStore.getState()).split(' '))
      for (const id of list) if (!still.has(id)) forgetBlocked(id)
    }
  }, [ids])
}
