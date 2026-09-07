import type { BotKind, ModelInfo } from '../api/types'

/** 模型清單快取值：陣列 = 取到了；null = 上次抓取失敗（畫面用靜態清單）。 */
export type ModelsCache = Record<string, ModelInfo[] | null>

/** 失敗後多久允許再打一次 `GET /api/models`（issue #26：失敗多半是暫時的）。 */
export const MODELS_RETRY_MS = 30_000

export function modelsKey(kind: BotKind, host: string, identity?: string | null): string {
  return `${kind}@${host || 'local'}@${identity || ''}`
}

/** 從快取 key 取回 host 段（`kind@host@identity`）。 */
export function modelsKeyHost(key: string): string {
  return key.split('@')[1] ?? 'local'
}

/**
 * 是否該（重新）抓取：沒抓過一定抓；上次失敗且已過 `MODELS_RETRY_MS` 也抓；
 * 有清單、或失敗還在冷卻期內，直接用快取。
 */
export function shouldFetchModels(
  cached: ModelInfo[] | null | undefined,
  failedAt: number | undefined,
  now: number,
): boolean {
  if (cached === undefined) return true
  if (cached !== null) return false
  return failedAt === undefined || now - failedAt >= MODELS_RETRY_MS
}

/** 把某台主機的所有模型快取（含失敗紀錄）清掉，讓下一次開面板重新抓。 */
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
