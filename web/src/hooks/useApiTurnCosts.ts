/** claude-api 每個 turn 的花費：每 bot 共用一份輪詢（不是每則訊息各自 fetch）。 */
import { useCallback, useEffect, useSyncExternalStore } from 'react'
import { fetchApiBotUsage, type ApiBotUsage, type ApiTurnCost } from '../api/apiBotUsage'

const SLOW_MS = 15_000
const FAST_MS = 3_000

interface Entry { turns: ApiTurnCost[]; last: ApiBotUsage['last']; subs: Set<() => void>; timer: ReturnType<typeof setTimeout> | null; fast: boolean }
const entries = new Map<string, Entry>()
const EMPTY: ApiTurnCost[] = []

function load(botId: string, e: Entry) {
  fetchApiBotUsage(botId).then(
    (u) => { e.turns = u.turns; e.last = u.last; e.subs.forEach((f) => f()) },
    () => {},
  ).finally(() => {
    if (!e.subs.size) { e.timer = null; return }
    const inFlight = e.fast || e.turns.some((x) => x.in_flight)
    e.timer = setTimeout(() => load(botId, e), inFlight ? FAST_MS : SLOW_MS)
  })
}

function subscribe(botId: string, cb: () => void) {
  let e = entries.get(botId)
  if (!e) { e = { turns: EMPTY, last: null, subs: new Set(), timer: null, fast: false }; entries.set(botId, e) }
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
