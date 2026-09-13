/**
 * 額度卡片上停用某身分／kind：其 bot 從側欄收起，到 reset 時間自動解除（見 docs/UI-DECISIONS.md）。
 * 純前端偏好、不進 zustand（免得被 WS 覆寫），用 `useSyncExternalStore` 共用。
 */
import { useSyncExternalStore } from 'react'
import { LOCAL_HOST } from '../api/types'
import type { BotKind } from '../api/types'
import { projectHostName, type StoreState } from './store'

export const QUOTA_DISABLED_KEY = 'am.disabledQuotaKeys'

/** key → 自動解除的時刻（epoch ms）；null = 那組額度沒有 reset 時間，只能手動解除。 */
export type DisabledMap = Readonly<Record<string, number | null>>

/** 以 host+kind+identity 記，不用額度 map key：cc0 額度可能落在裸 `claude` 下（`claudeQuotaKey`）。 */
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

/** 沒變動就回原物件（快照 identity 要穩）。 */
function prune(map: Record<string, number | null>, now: number): Record<string, number | null> {
  const live = Object.entries(map).filter(([, until]) => until === null || until > now)
  return live.length === Object.keys(map).length ? map : Object.fromEntries(live)
}

let disabled: Record<string, number | null> = prune(load(), Date.now())
const listeners = new Set<() => void>()
let expiryTimer: ReturnType<typeof setTimeout> | null = null

/** 用 timer 讓所有訂閱者同一刻解除，否則側欄放回 bot 了、額度格還灰著。 */
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
  // setTimeout 超過 2^31-1 ms 會立刻觸發。
  const delay = Math.min(Math.max(next - Date.now(), 250), 2 ** 31 - 1)
  expiryTimer = setTimeout(() => {
    expiryTimer = null
    const pruned = prune(disabled, Date.now())
    if (pruned !== disabled) publish(pruned)
    else scheduleExpiry()
  }, delay)
}

function publish(next: Record<string, number | null>) {
  disabled = next
  try {
    localStorage.setItem(QUOTA_DISABLED_KEY, JSON.stringify(next))
  } catch {
    /* 隱私模式：不記得而已 */
  }
  scheduleExpiry()
  for (const fn of listeners) fn()
}

scheduleExpiry()

function subscribe(fn: () => void) {
  listeners.add(fn)
  return () => listeners.delete(fn)
}

function snapshot(): DisabledMap {
  return disabled
}

export function useDisabledQuota(): DisabledMap {
  return useSyncExternalStore(subscribe, snapshot, snapshot)
}

/** `expiresAt`（epoch ms）在勾的當下定案，daemon 之後推新數字也不跳。 */
export function setQuotaDisabled(key: string, on: boolean, expiresAt: number | null): void {
  const next = { ...disabled }
  if (on) next[key] = expiresAt
  else delete next[key]
  publish(next)
}

/** 過期的已被 prune／timer 掃掉，呼叫端不必比時間。 */
export function isQuotaDisabled(map: DisabledMap, key: string): boolean {
  return key in map
}

/** 排序過（`useShallow` 才穩）。父列要自己與所有子 agent 都可收才收，免得子列沒地方掛。 */
export function quotaHiddenBotIds(state: StoreState, map: DisabledMap): string[] {
  if (Object.keys(map).length === 0) return []
  const hideable = new Set<string>()
  for (const b of state.bots) {
    // 執行中／有未讀的也收：留著的話整批 cc1 有未讀時勾了等於沒反應（見 docs/UI-DECISIONS.md）。
    const key = quotaDisableKey(projectHostName(state, b.project_id), b.kind, b.identity)
    if (isQuotaDisabled(map, key)) hideable.add(b.id)
  }
  const kept = new Set<string>()
  for (const b of state.bots) {
    if (!hideable.has(b.id) || b.parent_bot_id) continue
    if (state.bots.some((c) => c.parent_bot_id === b.id && !hideable.has(c.id))) kept.add(b.id)
  }
  return [...hideable].filter((id) => !kept.has(id)).sort()
}
