/**
 * Herdr 版本追蹤的共用狀態。
 *
 * 側欄那顆未讀點與環境設定裡的面板要同一份資料：各自 `useEffect` 抓一次就是兩個請求，而且會互相打臉
 * （一邊已讀、一邊還亮著）。這裡放一份 module-level 狀態 + 單飛的 in-flight promise，訂閱者共用。
 */
import { useEffect, useState } from 'react'
import type { HerdrUpdates } from '../api/herdrUpdates'
import { fetchHerdrUpdates, markHerdrUpdateSeen, refreshHerdrUpdates } from '../api/herdrUpdates'

let cached: HerdrUpdates | null = null
let inflight: Promise<HerdrUpdates> | null = null
const subscribers = new Set<(u: HerdrUpdates) => void>()

function publish(u: HerdrUpdates) {
  cached = u
  for (const cb of subscribers) cb(u)
}

/** 同時有兩個元件掛上來也只有一個請求。 */
function load(): Promise<HerdrUpdates> {
  if (!inflight) {
    inflight = fetchHerdrUpdates().finally(() => {
      inflight = null
    })
    void inflight.then(publish)
  }
  return inflight
}

export interface HerdrUpdatesState {
  data: HerdrUpdates | null
  /** 第一次載入中（已經有資料時重查不算，面板不該閃掉）。 */
  loading: boolean
  refreshing: boolean
  refresh: () => void
  markSeen: () => void
}

export function useHerdrUpdates(): HerdrUpdatesState {
  const [data, setData] = useState<HerdrUpdates | null>(cached)
  const [refreshing, setRefreshing] = useState(false)

  useEffect(() => {
    subscribers.add(setData)
    if (!cached) void load()
    return () => {
      subscribers.delete(setData)
    }
  }, [])

  return {
    data,
    loading: data === null,
    refreshing,
    refresh: () => {
      if (refreshing) return
      setRefreshing(true)
      void refreshHerdrUpdates()
        .then(publish)
        .finally(() => setRefreshing(false))
    },
    markSeen: () => {
      const v = data?.latest.version
      if (!v) return
      void markHerdrUpdateSeen(v).then(publish)
    },
  }
}
