/** claude-api 每個 turn 的花費與快取倒數依據：每 bot 共用一份輪詢（不是每則訊息或膠囊各自 fetch）。 */
import { useCallback, useEffect, useState, useSyncExternalStore } from 'react'
import { fetchApiBotUsage, type ApiBotUsage, type ApiTurnCost } from '../api/apiBotUsage'

const SLOW_MS = 15_000
const FAST_MS = 3_000

interface Entry {
  turns: ApiTurnCost[]
  last: ApiBotUsage['last']
  cache: ApiBotUsage['cache'] | null
  subs: Set<() => void>
  timer: ReturnType<typeof setTimeout> | null
  fast: boolean
  forTest?: boolean
}
const entries = new Map<string, Entry>()
const EMPTY: ApiTurnCost[] = []

function load(botId: string, e: Entry) {
  if (e.forTest) return
  fetchApiBotUsage(botId).then(
    (u) => {
      e.turns = u.turns
      e.last = u.last
      e.cache = u.cache
      e.subs.forEach((f) => f())
    },
    () => {},
  ).finally(() => {
    if (!e.subs.size) { e.timer = null; return }
    const inFlight = e.fast || e.turns.some((x) => x.in_flight)
    e.timer = setTimeout(() => load(botId, e), inFlight ? FAST_MS : SLOW_MS)
  })
}

function subscribe(botId: string, cb: () => void) {
  let e = entries.get(botId)
  if (!e) { e = { turns: EMPTY, last: null, cache: null, subs: new Set(), timer: null, fast: false }; entries.set(botId, e) }
  e.subs.add(cb)
  if (e.subs.size === 1 && !e.timer) load(botId, e)
  return () => {
    e!.subs.delete(cb)
    if (!e!.subs.size && e!.timer) { clearTimeout(e!.timer); e!.timer = null }
  }
}

/** `fast`＝回合進行中：一律 3 秒輪詢。回合狀態一變就補抓一次。 */
export function useApiTurnCosts(botId: string | undefined, fast = false): ApiTurnCost[] {
  const get = () => (botId ? entries.get(botId)?.turns ?? EMPTY : EMPTY)
  // subscribe 必須穩定：每次 render 換一個函式，useSyncExternalStore 會退訂再重訂，重訂又立刻 load 一次，
  // 結果每回完一次請求就再打一次（輪詢變成連發）。
  const sub = useCallback((cb: () => void) => (botId ? subscribe(botId, cb) : () => {}), [botId])
  const turns = useSyncExternalStore(sub, get, get)
  useEffect(() => {
    const e = botId ? entries.get(botId) : undefined
    if (!botId || !e) return
    e.fast = fast
    if (e.timer) { clearTimeout(e.timer); load(botId, e) }
    return () => { e.fast = false }
  }, [botId, fast])
  return turns
}

/**
 * 最近一次 recorded 呼叫（含它讀到的整段 context token 數）：冷掉時「下一則約重寫多少」用。
 * 跟 `useApiTurnCosts` 共用同一份輪詢；`botId` 傳 undefined（不是 claude-api）就不訂閱、不打 API。
 */
export function useApiLastCall(botId: string | undefined): ApiBotUsage['last'] {
  const sub = useCallback((cb: () => void) => (botId ? subscribe(botId, cb) : () => {}), [botId])
  const get = () => (botId ? entries.get(botId)?.last ?? null : null)
  return useSyncExternalStore(sub, get, get)
}

/** 取得指定 bot 目前快取的 cache 資訊（同步讀取，未抓取前為 null）。 */
export function getApiBotCache(botId: string): ApiBotUsage['cache'] | null {
  return entries.get(botId)?.cache ?? null
}

/** 立即重新整理指定 bot 的 API 用量與快取（若有訂閱者且未在載入中）。 */
export function refreshApiBotUsage(botId: string) {
  const e = entries.get(botId)
  if (e && e.subs.size > 0) {
    if (e.timer) {
      clearTimeout(e.timer)
      e.timer = null
    }
    load(botId, e)
  }
}

/** 單顆 bot 快取訂閱 hook。 */
export function useApiBotCache(botId: string | undefined): ApiBotUsage['cache'] | null {
  const sub = useCallback((cb: () => void) => (botId ? subscribe(botId, cb) : () => {}), [botId])
  const get = () => (botId ? entries.get(botId)?.cache ?? null : null)
  return useSyncExternalStore(sub, get, get)
}

/**
 * 釘選的 claude-api bots 快取輪詢訂閱：多顆 bot 共用一份輪詢，每顆至多一個計時器（idle 15s、in-flight 3s）。
 * 只要有任何一顆更新就推進版本號觸發重新計算。
 */
export function useApiBotCacheSub(botIds: readonly string[]): number {
  const [ver, setVer] = useState(0)
  const key = botIds.slice().sort().join(',')
  useEffect(() => {
    if (!botIds.length) return
    const bump = () => setVer((v) => v + 1)
    const unsubs = botIds.map((id) => subscribe(id, bump))
    return () => unsubs.forEach((u) => u())
  }, [key])
  return ver
}

/** 測試用：直接注入特定 bot 的快取狀態並通知訂閱者。 */
export function setApiBotCacheForTest(botId: string, cache: ApiBotUsage['cache'] | null) {
  let e = entries.get(botId)
  if (!e) {
    e = { turns: EMPTY, last: null, cache, subs: new Set(), timer: null, fast: false, forTest: true }
    entries.set(botId, e)
  } else {
    e.cache = cache
    e.forTest = true
  }
  e.subs.forEach((f) => f())
}

/** 測試用：清除所有狀態與計時器。 */
export function clearApiBotEntriesForTest() {
  for (const e of entries.values()) {
    if (e.timer) clearTimeout(e.timer)
  }
  entries.clear()
}
