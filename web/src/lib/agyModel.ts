/**
 * agy 模型排序規則（使用者 2026-11-20）：
 * 1) 依家族由便宜到貴排序：flash < hydrogen < argon < neon；未知家族排最後、照原順序。
 * 2) 同家族依版號由舊到新。
 * 3) 同版號依強度 low < medium < high < xhigh 排序。
 * 4) 認不得的 id 排最後原樣。
 */

export const AGY_FAMILY_ORDER: Record<string, number> = {
  flash: 0,
  hydrogen: 1,
  argon: 2,
  neon: 3,
}

export const AGY_EFFORT_ORDER: Record<string, number> = {
  low: 0,
  medium: 1,
  high: 2,
  xhigh: 3,
}

export interface ParsedAgyModel {
  version: string
  versionNumbers: number[]
  family: string
  familyRank: number
  effort?: string
  effortRank: number
}

/**
 * 解析 agy 模型 id：`gemini-<版號>-<家族>[-<強度>]`
 * 認不得格式時回 null。
 */
export function parseAgyModel(id: string): ParsedAgyModel | null {
  const m = /^gemini-(\d+(?:\.\d+)*)-([a-z0-9]+)(?:-([a-z0-9]+))?$/i.exec(id)
  if (!m) return null
  const version = m[1]
  const family = m[2].toLowerCase()
  const effort = m[3]?.toLowerCase()
  const versionNumbers = (version.match(/\d+/g) ?? []).map(Number)
  const familyRank = Object.prototype.hasOwnProperty.call(AGY_FAMILY_ORDER, family)
    ? AGY_FAMILY_ORDER[family]
    : 99
  const effortRank = effort
    ? (Object.prototype.hasOwnProperty.call(AGY_EFFORT_ORDER, effort)
        ? AGY_EFFORT_ORDER[effort]
        : 99)
    : -1
  return {
    version,
    versionNumbers,
    family,
    familyRank,
    effort,
    effortRank,
  }
}

/**
 * 強度標籤格式化（首字大寫，xhigh 格式化為 XHigh）：
 * low → Low, medium → Medium, high → High, xhigh → XHigh
 */
export function formatAgyEffortLabel(effort: string): string {
  if (effort.toLowerCase() === 'xhigh') return 'XHigh'
  return effort.charAt(0).toUpperCase() + effort.slice(1).toLowerCase()
}

/**
 * 按鈕顯示字（比照 codex，由 id 推導）：
 * `gemini-<版號>-<家族>[-<強度>]` → 「<版號>-<家族首字大寫> <強度>」
 * 例：`gemini-3.8-flash-low` → 「3.8-Flash Low」
 *     `gemini-4-neon-xhigh` → 「4-Neon XHigh」
 *     `gemini-4-argon` → 「4-Argon」
 * 認不得格式的 id 照舊原樣顯示。
 */
export function formatAgyModelLabel(id: string): string {
  const parsed = parseAgyModel(id)
  if (!parsed) return id
  const capitalizedFamily = parsed.family.charAt(0).toUpperCase() + parsed.family.slice(1)
  const effortPart = parsed.effort ? ` ${formatAgyEffortLabel(parsed.effort)}` : ''
  return `${parsed.version}-${capitalizedFamily}${effortPart}`
}

/**
 * 比較兩版本號（由舊到新）：
 * e.g. 3.8 < 4 < 4.1
 */
export function compareVersions(av: readonly number[], bv: readonly number[]): number {
  for (let i = 0; i < Math.max(av.length, bv.length); i++) {
    const delta = (av[i] ?? -1) - (bv[i] ?? -1)
    if (delta !== 0) return delta
  }
  return 0
}

/**
 * 建立未知家族的出現順序表，保證排序傳遞性與穩定性：
 * 已知家族固定 rank（0..3），未知家族依首次出現順序賦予 rank（100, 101, ...）。
 */
export function buildAgyFamilyOrder(ids: readonly string[]): Map<string, number> {
  const order = new Map<string, number>()
  for (const [k, v] of Object.entries(AGY_FAMILY_ORDER)) {
    order.set(k, v)
  }
  let nextUnknownRank = 100
  for (const id of ids) {
    const p = parseAgyModel(id)
    if (p && !order.has(p.family)) {
      order.set(p.family, nextUnknownRank++)
    }
  }
  return order
}

/**
 * agy 模型排序：
 * - 家族由便宜到貴：flash < hydrogen < argon < neon（未知家族排最後、照原順序）
 * - 同家族依版號由舊到新、再依強度 low < medium < high < xhigh
 * - 認不得的 id 排最後原樣
 */
export function compareAgyModels(
  aId: string,
  bId: string,
  familyOrder?: ReadonlyMap<string, number>,
): number {
  const pa = parseAgyModel(aId)
  const pb = parseAgyModel(bId)
  if (pa && pb) {
    const rankA = familyOrder?.get(pa.family) ?? pa.familyRank
    const rankB = familyOrder?.get(pb.family) ?? pb.familyRank
    if (rankA !== rankB) {
      return rankA - rankB
    }
    // 若沒有 familyOrder 且都是未知家族且家族名不同，以名稱穩定比較保證傳遞性
    if (pa.familyRank === 99 && pa.family !== pb.family) {
      return pa.family.localeCompare(pb.family)
    }
    // 同家族：先依版號由舊到新
    const ver = compareVersions(pa.versionNumbers, pb.versionNumbers)
    if (ver !== 0) return ver
    // 同版號：再依強度 low < medium < high < xhigh
    if (pa.effortRank !== pb.effortRank) {
      return pa.effortRank - pb.effortRank
    }
    return 0
  }
  if (pa && !pb) return -1
  if (!pa && pb) return 1
  return 0
}

/**
 * 對 agy 模型 id 清單排序
 */
export function sortAgyModelIds(ids: readonly string[]): string[] {
  const familyOrder = buildAgyFamilyOrder(ids)
  return ids
    .map((id, i) => ({ id, i }))
    .sort((a, b) => compareAgyModels(a.id, b.id, familyOrder) || a.i - b.i)
    .map((x) => x.id)
}
