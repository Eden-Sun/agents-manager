/**
 * 額度卡片上停用某身分／kind：其 bot 從側欄收起，到 reset 時間自動解除（見 docs/UI-DECISIONS.md）。
 * 純前端偏好、不進 zustand（免得被 WS 覆寫），用 `useSyncExternalStore` 共用。
 */
import { useSyncExternalStore } from 'react'
import { LOCAL_HOST } from '../api/types'
import type { BotKind } from '../api/types'
import { projectHostName, useStore, type StoreState } from './store'

export const QUOTA_DISABLED_KEY = 'am.disabledQuotaKeys'

/** key → 自動解除的時刻（epoch ms）；null = 那組額度沒有 reset 時間，只能手動解除。 */
export type DisabledMap = Readonly<Record<string, number | null>>

/**
 * 以 host+kind+identity 記，刻意不用額度 map 的 key：落點會變（`quotaLookup.quotaBaseKey`——
 * 共用預設帳號的身分會退回裸 `claude`），拿會變的東西當偏好的鍵，數字一落位偏好就對不回來。
 */
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
  // 勾停用只動這裡的 map，store 不會自己變，投影要自己推一次。
  syncHiddenBots()
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
    // 正在看的那顆永遠留著：主面板還開著它的對話，側欄那一列卻不見了，只能打字搜尋才找得回來。
    if (b.id === state.selectedBotId) continue
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

/**
 * 「側欄看得到哪些 bot」要有單一定義。寫入端仍是這個模組（不進 zustand 就不會被 WS 覆寫），
 * 但結果要投影進 store：⌥↑／⌥↓ 走的是 `orderedBotIds`、標題列晶片與分頁標題的 `(N)` 走的是
 * `botUnread`，它們以前都不知道側欄少了一批 bot——點得到卻找不到那一列、數字也對不起來。
 */
function syncHiddenBots() {
  const st = useStore.getState()
  const next = quotaHiddenBotIds(st, disabled)
  const cur = st.hiddenBotIds
  if (next.length === cur.length && next.every((id, i) => id === cur[i])) return
  useStore.setState({ hiddenBotIds: next })
}

useStore.subscribe(syncHiddenBots)
syncHiddenBots()
