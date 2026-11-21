import { useSyncExternalStore } from 'react'
import { LOCAL_HOST } from '../api/types'
import type { BotKind } from '../api/types'
import { projectHostName, useStore, type StoreState } from './store'

export const QUOTA_DISABLED_KEY = 'am.disabledQuotaKeys'

export type DisabledMap = Readonly<Record<string, number | null>>

export function quotaDisableKey(host: string, kind: BotKind, identity: string | null): string {
  return `${host || LOCAL_HOST}|${kind}|${identity ?? ''}`
}

function load(): Record<string, number | null> {
  try {
    const raw = localStorage.getItem(QUOTA_DISABLED_KEY)
    if (!raw) return {}
    const parsed: unknown = JSON.parse(raw)
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {}
    const out: Record<string, number | null> = {}
    for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
      if (v === null || typeof v === 'number') out[k] = v
    }
    return out
  } catch {
    return {}
  }
}

function prune(map: Record<string, number | null>, now: number): Record<string, number | null> {
  const live = Object.entries(map).filter(([, until]) => until === null || until > now)
  return live.length === Object.keys(map).length ? map : Object.fromEntries(live)
}

let disabled: Record<string, number | null> = prune(load(), Date.now())
const listeners = new Set<() => void>()
let expiryTimer: ReturnType<typeof setTimeout> | null = null

function scheduleExpiry() {
  if (expiryTimer !== null) {
    clearTimeout(expiryTimer)
    expiryTimer = null
  }
  let next: number | null = null
  for (const until of Object.values(disabled)) {
    if (until === null) continue
    if (next === null || until < next) next = until
  }
  if (next === null) return
  const delay = Math.min(Math.max(next - Date.now(), 250), 2 ** 31 - 1)
  expiryTimer = setTimeout(() => {
    expiryTimer = null
    const pruned = prune(disabled, Date.now())
    if (pruned !== disabled) publish(pruned)
    else scheduleExpiry()
  }, delay)
}

export function mergeDisabled(
  prev: Record<string, number | null>,
  next: Record<string, number | null>,
  disk: Record<string, number | null>,
): Record<string, number | null> {
  const out = { ...disk }
  for (const k of Object.keys(prev)) if (!(k in next)) delete out[k]
  for (const [k, v] of Object.entries(next)) if (!(k in prev) || prev[k] !== v) out[k] = v
  return out
}

function sameMap(a: Record<string, number | null>, b: Record<string, number | null>): boolean {
  const ak = Object.keys(a)
  return ak.length === Object.keys(b).length && ak.every((k) => k in b && a[k] === b[k])
}

function adopt(next: Record<string, number | null>) {
  disabled = next
  scheduleExpiry()
  syncHiddenBots()
  for (const fn of listeners) fn()
}

function publish(next: Record<string, number | null>) {
  let merged = next
  try {
    merged = prune(mergeDisabled(disabled, next, load()), Date.now())
    localStorage.setItem(QUOTA_DISABLED_KEY, JSON.stringify(merged))
  } catch {
  }
  adopt(merged)
}

function onStorage(e: StorageEvent) {
  if (e.key !== null && e.key !== QUOTA_DISABLED_KEY) return
  const disk = prune(load(), Date.now())
  if (!sameMap(disk, disabled)) adopt(disk)
}

let storageWindow: Window | null = null
function listenStorage() {
  if (typeof window === 'undefined') return
  if (storageWindow !== window) {
    storageWindow?.removeEventListener('storage', onStorage)
    storageWindow = window
  }
  window.addEventListener('storage', onStorage)
}
listenStorage()

scheduleExpiry()

function subscribe(fn: () => void) {
  listenStorage()
  listeners.add(fn)
  return () => listeners.delete(fn)
}

function snapshot(): DisabledMap {
  return disabled
}

export function useDisabledQuota(): DisabledMap {
  return useSyncExternalStore(subscribe, snapshot, snapshot)
}

export function setQuotaDisabled(key: string, on: boolean, expiresAt: number | null): void {
  const next = { ...disabled }
  if (on) next[key] = expiresAt
  else delete next[key]
  publish(next)
}

export function isQuotaDisabled(map: DisabledMap, key: string): boolean {
  return key in map
}

export function quotaHiddenBotIds(state: StoreState, map: DisabledMap): string[] {
  if (Object.keys(map).length === 0) return []
  const hideable = new Set<string>()
  for (const b of state.bots) {
    if (b.id === state.selectedBotId) continue
    const key = quotaDisableKey(projectHostName(state, b.project_id), b.kind, b.identity)
    if (isQuotaDisabled(map, key)) hideable.add(b.id)
  }
  const childrenOf = new Map<string, string[]>()
  for (const b of state.bots) {
    if (b.parent_bot_id) childrenOf.set(b.parent_bot_id, [...(childrenOf.get(b.parent_bot_id) ?? []), b.id])
  }
  const keepsDescendant = (id: string, seen: Set<string>): boolean => {
    if (seen.has(id)) return false
    seen.add(id)
    return (childrenOf.get(id) ?? []).some((c) => !hideable.has(c) || keepsDescendant(c, seen))
  }
  return [...hideable].filter((id) => !keepsDescendant(id, new Set())).sort()
}

function syncHiddenBots() {
  const st = useStore.getState()
  const next = quotaHiddenBotIds(st, disabled)
  const cur = st.hiddenBotIds
  if (next.length === cur.length && next.every((id, i) => id === cur[i])) return
  useStore.setState({ hiddenBotIds: next })
}

useStore.subscribe(syncHiddenBots)
syncHiddenBots()
