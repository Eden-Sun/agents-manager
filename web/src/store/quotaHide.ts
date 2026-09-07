/**
 * 「把某個身分／kind 暫時停用」——額度快用完時，在頂端額度卡片上勾掉那一格，它底下的
 * bot 就先從側欄收起來，等那組額度的視窗 reset 時間到了自動解除（UI 取捨見
 * docs/UI-DECISIONS.md）。
 *
 * 狀態自己存一份、不進 zustand：這是純前端的檢視偏好（跟 `am.collapsedProjects` 同一類），
 * daemon 不需要知道，也不該讓它跟著 store 的 state 一起被 WS 覆寫。用
 * `useSyncExternalStore` 讓額度卡片與側欄看到同一份。
 */
import { useSyncExternalStore } from 'react'
import { LOCAL_HOST } from '../api/types'
import type { BotKind } from '../api/types'
import { projectHostName, type StoreState } from './store'

export const QUOTA_DISABLED_KEY = 'am.disabledQuotaKeys'

/** key → 自動解除的時刻（epoch ms）；null = 那組額度沒有 reset 時間，只能手動解除。 */
export type DisabledMap = Readonly<Record<string, number | null>>

/**
 * 停用是記在「哪一台主機的哪個身分」上，不是記在額度 map 的 key 上——cc0 的額度可能落在
 * 裸的 `claude` 底下（見 QuotaStrip 的 `claudeQuotaKey`），用 kind+identity 對 bot 才對得準。
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

/** 已經過了自動解除時刻的都丟掉；沒有變動就回原本那個物件（快照的 identity 要穩）。 */
function prune(map: Record<string, number | null>, now: number): Record<string, number | null> {
  const live = Object.entries(map).filter(([, until]) => until === null || until > now)
  return live.length === Object.keys(map).length ? map : Object.fromEntries(live)
}

let disabled: Record<string, number | null> = prune(load(), Date.now())
const listeners = new Set<() => void>()
let expiryTimer: ReturnType<typeof setTimeout> | null = null

/**
 * 排一個 timer 在最近一次自動解除的時刻把它掃掉。時間到時所有訂閱者（額度卡片、條上那格、
 * 側欄清單）在**同一刻**一起更新——不然側欄已經把 bot 放回來了，條上那格還灰著一分鐘。
 */
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
  // setTimeout 的上限是 2^31-1 ms（約 24.8 天），超過會立刻觸發；夾一下比較保險。
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
    /* 隱私模式寫不進去就算了，這次不記得而已 */
  }
  scheduleExpiry()
  for (const fn of listeners) fn()
}

scheduleExpiry()

function subscribe(fn: () => void) {
  listeners.add(fn)
  return () => listeners.delete(fn)
}

/** 快照的 identity 只在真的改動時才變，`useSyncExternalStore` 才不會判定每次都變。 */
function snapshot(): DisabledMap {
  return disabled
}

export function useDisabledQuota(): DisabledMap {
  return useSyncExternalStore(subscribe, snapshot, snapshot)
}

/**
 * 勾／取消勾一格。`expiresAt` 是那組額度最近一次 reset 的時刻（epoch ms）——勾的當下就
 * 算好存起來，之後就算 daemon 推了新數字進來，解除時間也不會跟著跳。
 */
export function setQuotaDisabled(key: string, on: boolean, expiresAt: number | null): void {
  const next = { ...disabled }
  if (on) next[key] = expiresAt
  else delete next[key]
  publish(next)
}

/**
 * 這一格現在是不是停用中。過期的在 `prune` 就被掃掉了（載入時一次，之後由 timer 負責），
 * 所以留在 map 裡的都還有效——呼叫端不必自己比時間。
 */
export function isQuotaDisabled(map: DisabledMap, key: string): boolean {
  return key in map
}

/**
 * 現在該收起來的 bot id（排序過，`useShallow` 才比得穩）。
 *
 * 父子規則：父列只有在**它自己和它底下每個子 agent 都可以收**的時候才收走。否則留著父列，
 * 可以收的子列還是各自收——不然子 agent 會連掛的地方都沒有。
 */
export function quotaHiddenBotIds(state: StoreState, map: DisabledMap): string[] {
  if (Object.keys(map).length === 0) return []
  const hideable = new Set<string>()
  for (const b of state.bots) {
    // 停用是明講的動作（「這個帳號的先別給我看」），所以不留例外：執行中與有未讀的也一起收。
    // 早期版本把它們留在清單上，結果實測時整批 cc1 都有未讀，勾了等於沒反應（見
    // docs/UI-DECISIONS.md）。收掉多少由專案卡片上那行「N 個 Bot 已隱藏」交代。
    const key = quotaDisableKey(projectHostName(state, b.project_id), b.kind, b.identity)
    if (isQuotaDisabled(map, key)) hideable.add(b.id)
  }
  const kept = new Set<string>()
  for (const b of state.bots) {
    if (!hideable.has(b.id) || b.parent_bot_id) continue
    // 父列：底下有任何一個子 agent 要留著，它就得跟著留著。
    if (state.bots.some((c) => c.parent_bot_id === b.id && !hideable.has(c.id))) kept.add(b.id)
  }
  return [...hideable].filter((id) => !kept.has(id)).sort()
}
