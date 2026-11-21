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

export function useApiTurnCosts(botId: string | undefined, fast = false): ApiTurnCost[] {
  const get = () => (botId ? entries.get(botId)?.turns ?? EMPTY : EMPTY)
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

export function useApiLastCall(botId: string | undefined): ApiBotUsage['last'] {
  const sub = useCallback((cb: () => void) => (botId ? subscribe(botId, cb) : () => {}), [botId])
  const get = () => (botId ? entries.get(botId)?.last ?? null : null)
  return useSyncExternalStore(sub, get, get)
}

export function getApiBotCache(botId: string): ApiBotUsage['cache'] | null {
  return entries.get(botId)?.cache ?? null
}

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

export function useApiBotCache(botId: string | undefined): ApiBotUsage['cache'] | null {
  const sub = useCallback((cb: () => void) => (botId ? subscribe(botId, cb) : () => {}), [botId])
  const get = () => (botId ? entries.get(botId)?.cache ?? null : null)
  return useSyncExternalStore(sub, get, get)
}

export function useApiBotCacheSub(botIds: readonly string[]): number {
  const [ver, setVer] = useState(0)
  const key = botIds.slice().sort().join(',')
  useEffect(() => {
    if (!key) return
    const bump = () => setVer((v) => v + 1)
    const unsubs = key.split(',').map((id) => subscribe(id, bump))
    return () => unsubs.forEach((u) => u())
  }, [key])
  return ver
}

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

export function clearApiBotEntriesForTest() {
  for (const e of entries.values()) {
    if (e.timer) clearTimeout(e.timer)
  }
  entries.clear()
}
