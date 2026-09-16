/**
 * `store/herdrUpdates.ts` 的 React 外殼：接上真的計時器、真的可見性事件與真的 API。
 *
 * 側欄那顆未讀提示與環境設定裡的面板共用同一個 store，所以只有一份請求，而且按掉之後兩邊一起消失。
 */
import { useEffect, useState } from 'react'
import type { HerdrUpdates } from '../api/herdrUpdates'
import { fetchHerdrUpdates, markHerdrUpdateSeen, refreshHerdrUpdates } from '../api/herdrUpdates'
import { createHerdrUpdatesStore } from '../store/herdrUpdates'

/** 分頁重新可見、或網路回來：兩者都代表「剛剛那段時間我可能漏看了」。 */
function onWake(fn: () => void): () => void {
  const visible = () => {
    if (document.visibilityState === 'visible') fn()
  }
  document.addEventListener('visibilitychange', visible)
  window.addEventListener('online', fn)
  return () => {
    document.removeEventListener('visibilitychange', visible)
    window.removeEventListener('online', fn)
  }
}

const store = createHerdrUpdatesStore({
  fetchCache: fetchHerdrUpdates,
  refreshNow: refreshHerdrUpdates,
  markSeen: markHerdrUpdateSeen,
  setInterval: (fn, ms) => window.setInterval(fn, ms),
  clearInterval: (h) => window.clearInterval(h as number),
  now: () => Date.now(),
  onWake,
  isVisible: () => document.visibilityState === 'visible',
})

export interface HerdrUpdatesState {
  data: HerdrUpdates | null
  /** 第一次載入中（已經有資料時重查不算，面板不該閃掉）。 */
  loading: boolean
  refreshing: boolean
  refresh: () => void
  markSeen: () => void
}

export function useHerdrUpdates(): HerdrUpdatesState {
  const [data, setData] = useState<HerdrUpdates | null>(() => store.get())
  const [refreshing, setRefreshing] = useState(false)

  useEffect(() => store.subscribe(setData), [])

  return {
    data,
    loading: data === null,
    refreshing,
    refresh: () => {
      if (refreshing) return
      setRefreshing(true)
      void store.refresh().finally(() => setRefreshing(false))
    },
    markSeen: () => void store.markSeenNow(),
  }
}
