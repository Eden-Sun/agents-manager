import type { BotKind, ModelInfo } from '../api/types'

/** null = 上次抓取失敗（畫面用靜態清單）。 */
export type ModelsCache = Record<string, ModelInfo[] | null>

/** issue #26：失敗多半是暫時的，冷卻後重試。 */
export const MODELS_RETRY_MS = 30_000

export function modelsKey(kind: BotKind, host: string, identity?: string | null): string {
  return `${kind}@${host || 'local'}@${identity || ''}`
}

export function modelsKeyHost(key: string): string {
  return key.split('@')[1] ?? 'local'
}

export function shouldFetchModels(
  cached: ModelInfo[] | null | undefined,
  failedAt: number | undefined,
  now: number,
): boolean {
  if (cached === undefined) return true
  if (cached !== null) return false
  return failedAt === undefined || now - failedAt >= MODELS_RETRY_MS
}

/** 含失敗紀錄。 */
export function dropHostModels<T>(cache: Record<string, T>, host: string): Record<string, T> {
  const target = host || 'local'
  let changed = false
  const next: Record<string, T> = {}
  for (const [key, value] of Object.entries(cache)) {
    if (modelsKeyHost(key) === target) changed = true
    else next[key] = value
  }
  return changed ? next : cache
}

/** 「最後抓到的」清單：每次 `fetchModels` 成功就存，API 載入中或失敗時畫面先用它，不退回寫死的清單（使用者 2026-11-09）。 */
const LAST_PREFIX = 'am.models.last:'

function storage(): Storage | null {
  try {
    return typeof localStorage === 'undefined' ? null : localStorage
  } catch {
    return null
  }
}

export function saveLastModels(key: string, list: ModelInfo[]): void {
  try {
    storage()?.setItem(LAST_PREFIX + key, JSON.stringify({ at: Date.now(), list }))
  } catch {
    /* 滿了或被擋：當作沒存 */
  }
}

function readLast(raw: string | null): { at: number; list: ModelInfo[] } | null {
  if (!raw) return null
  try {
    const v = JSON.parse(raw) as { at?: unknown; list?: unknown }
    return typeof v.at === 'number' && Array.isArray(v.list) && v.list.length > 0 ? { at: v.at, list: v.list as ModelInfo[] } : null
  } catch {
    return null
  }
}

/** 同 key 的最後一份；沒有就同 kind 任一 key 最新的那份；都沒有（或讀不到）＝ null。 */
export function lastKnownModels(kind: BotKind, key: string): ModelInfo[] | null {
  try {
    const st = storage()
    if (!st) return null
    const exact = readLast(st.getItem(LAST_PREFIX + key))
    if (exact) return exact.list
    let best: { at: number; list: ModelInfo[] } | null = null
    for (let i = 0; i < st.length; i++) {
      const k = st.key(i)
      if (!k?.startsWith(`${LAST_PREFIX}${kind}@`)) continue
      const v = readLast(st.getItem(k))
      if (v && (!best || v.at > best.at)) best = v
    }
    return best?.list ?? null
  } catch {
    return null
  }
}
