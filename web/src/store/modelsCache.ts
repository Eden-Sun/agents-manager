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
