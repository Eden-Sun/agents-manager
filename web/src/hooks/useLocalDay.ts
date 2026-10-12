import { useSyncExternalStore } from 'react'
import { nextLocalMidnight, startOfLocalDay } from '../lib/msgTime'

/**
 * 「今天」的開始（本地 0 點），跨午夜會換。訊息時間標籤靠它重算：泡泡是 memo 的，沒有這個，跨日後畫面上的
 * `23:50` 會一直停在「今天」的寫法，不會換成 `12/31 23:50`。整個模組共用一個計時器，不是每則泡泡一個。
 */
let today = startOfLocalDay(Date.now())
const listeners = new Set<() => void>()
let timer: ReturnType<typeof setTimeout> | null = null

function refresh() {
  const next = startOfLocalDay(Date.now())
  if (next !== today) {
    today = next
    for (const fn of listeners) fn()
  }
  arm()
}

function arm() {
  if (timer !== null) clearTimeout(timer)
  timer = null
  if (listeners.size === 0) return
  // 在下一個午夜叫醒；分頁在背景被節流時，回到前景也會再對一次（見下面的 visibilitychange）。
  timer = setTimeout(refresh, Math.max(1000, nextLocalMidnight(Date.now()) - Date.now()))
}

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden) refresh()
  })
}

function subscribe(fn: () => void): () => void {
  listeners.add(fn)
  refresh()
  return () => {
    listeners.delete(fn)
    if (listeners.size === 0 && timer !== null) {
      clearTimeout(timer)
      timer = null
    }
  }
}

/** 目前本地「今天」的開始（epoch ms）；跨午夜時回傳值會變，元件就會重畫。拿去當 `msgTimeLabel` 的 `now`。 */
export function useLocalDay(): number {
  return useSyncExternalStore(subscribe, () => today, () => today)
}
