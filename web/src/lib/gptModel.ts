/**
 * GPT 模型命名與排序規則（使用者 2026-11-19）：
 * 1) codex 與 openai-api 命名統一，由模型 id 推導：
 *    - 按鈕標籤：`gpt-<版本>-<名稱>` → 「<版本>-<名稱首字大寫>」（6-Luna、6.1-Terra、6.2-Sol、6.2-Astra）
 *    - 側欄短名：`gpt-<版本>-<名稱>` → 「<版本>-<名稱>」小寫（6.2-astra）
 *    - 認不得格式的 id 照舊原樣顯示。
 * 2) 排序依家族由便宜到貴：luna < terra < sol < astra；同家族由舊到新。
 */

export const GPT_FAMILY_ORDER: Record<string, number> = {
  luna: 0,
  terra: 1,
  sol: 2,
  astra: 3,
}

export interface ParsedGptModel {
  version: string
  versionNumbers: number[]
  family: string
  familyRank: number
  suffix?: string
}

/**
 * 解析 GPT 模型 id：`gpt-<版本>-<家族/名稱>[-<後綴>]`
 * 認不得格式時回 null。
 */
export function parseGptModel(id: string): ParsedGptModel | null {
  const m = /^gpt-(\d+(?:\.\d+)*)-([a-z0-9]+)(?:-(.*))?$/i.exec(id)
  if (!m) return null
  const version = m[1]
  const family = m[2].toLowerCase()
  const versionNumbers = (version.match(/\d+/g) ?? []).map(Number)
  const familyRank = Object.prototype.hasOwnProperty.call(GPT_FAMILY_ORDER, family)
    ? GPT_FAMILY_ORDER[family]
    : 99
  return {
    version,
    versionNumbers,
    family,
    familyRank,
    suffix: m[3],
  }
}

/**
 * 按鈕顯示字（codex 與 openai-api 統一，由 id 推導，不用 display_name）：
 * `gpt-<版本>-<名稱>` → 「<版本>-<名稱首字大寫>」
 * 認不得格式的 id 照舊原樣顯示。
 */
export function formatGptModelLabel(id: string): string {
  const parsed = parseGptModel(id)
  if (!parsed) return id
  const capitalizedFamily = parsed.family.charAt(0).toUpperCase() + parsed.family.slice(1)
  const suffix = parsed.suffix ? `-${parsed.suffix}` : ''
  return `${parsed.version}-${capitalizedFamily}${suffix}`
}

/**
 * 側欄短名格式（codex 與 openai-api 統一，由 id 推導）：
 * `gpt-<版本>-<名稱>` → 「<版本>-<名稱>」小寫
 * 認不得格式的 id 照舊原樣顯示。
 */
export function formatGptShortModel(id: string): string {
  const parsed = parseGptModel(id)
  if (!parsed) return id
  const suffix = parsed.suffix ? `-${parsed.suffix.toLowerCase()}` : ''
  return `${parsed.version}-${parsed.family}${suffix}`
}

/**
 * 比較兩版本號（由舊到新）：
 * e.g. 6 < 6.1 < 6.2
 */
export function compareVersions(av: readonly number[], bv: readonly number[]): number {
  for (let i = 0; i < Math.max(av.length, bv.length); i++) {
    const delta = (av[i] ?? -1) - (bv[i] ?? -1)
    if (delta !== 0) return delta
  }
  return 0
}

/**
 * GPT 模型排序（由便宜到貴：luna < terra < sol < astra；同家族依版本號由舊到新）：
 */
export function compareGptModels(aId: string, bId: string): number {
  const pa = parseGptModel(aId)
  const pb = parseGptModel(bId)
  if (pa && pb) {
    if (pa.familyRank !== pb.familyRank) {
      return pa.familyRank - pb.familyRank
    }
    const ver = compareVersions(pa.versionNumbers, pb.versionNumbers)
    if (ver !== 0) return ver
    return aId.localeCompare(bId)
  }
  if (pa && !pb) return -1
  if (!pa && pb) return 1
  return aId.localeCompare(bId)
}

/**
 * 對 GPT 模型 id 清單排序
 */
export function sortGptModelIds(ids: readonly string[]): string[] {
  return [...ids].sort(compareGptModels)
}
