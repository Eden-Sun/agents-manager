/**
 * agy 模型分組（方案 A，reports/plan-agy-model-effort.md）：
 * 後端仍是每個 CLI slug 一筆（`gemini-4-neon-high`），這裡把它們依基礎模型分組，給「模型」與「強度」兩排用。
 * 純函式、不碰 DOM；存值仍是完整 slug，強度列只是選變體。
 */
import type { ModelInfo } from '../api/types'
import { AGY_EFFORT_ORDER, formatAgyModelLabel, isAgyOrchestrator, parseAgyModel } from './agyModel'

export interface AgyModelGroup {
  /** 分組鍵：`gemini-<版號>-<家族>`（有強度的變體）或 slug 本身（Orchestrator、無強度、未知格式）。 */
  key: string
  /** 模型列的按鈕字（`3.8-Flash`、`4-Neon`、`Orchestrator Neon`）。 */
  label: string
  /** 強度列要畫的等級，依 `low < medium < high < xhigh` 排序；空陣列＝不畫強度列。 */
  efforts: string[]
  /** 強度 → 該強度的完整 slug 模型。 */
  variants: Map<string, ModelInfo>
  /** 沒有強度的群組（Orchestrator、無強度 slug、未知格式）唯一的那一顆。 */
  single: ModelInfo | null
  /** 群組內任一顆是 CLI 預設（`is_default`）。 */
  isDefault: boolean
}

/** 依輸入順序分組（呼叫端先排好序，群組順序就是第一次出現的順序）。 */
export function groupAgyModels(models: readonly ModelInfo[]): AgyModelGroup[] {
  const groups = new Map<string, AgyModelGroup>()
  for (const m of models) {
    const parsed = parseAgyModel(m.id)
    const withEffort = Boolean(parsed?.effort) && !isAgyOrchestrator(m.id)
    const key = withEffort && parsed ? `gemini-${parsed.version}-${parsed.family}` : m.id
    let g = groups.get(key)
    if (!g) {
      const label = withEffort ? formatAgyModelLabel(key) : parsed || isAgyOrchestrator(m.id) ? formatAgyModelLabel(m.id) : m.display_name || m.id
      g = { key, label, efforts: [], variants: new Map(), single: null, isDefault: false }
      groups.set(key, g)
    }
    if (withEffort && parsed?.effort) {
      g.variants.set(parsed.effort, m)
    } else {
      g.single = m
    }
    if (m.is_default) g.isDefault = true
  }
  for (const g of groups.values()) {
    g.efforts = [...g.variants.keys()].sort((a, b) => rank(a) - rank(b))
  }
  return [...groups.values()]
}

function rank(effort: string): number {
  return Object.prototype.hasOwnProperty.call(AGY_EFFORT_ORDER, effort) ? AGY_EFFORT_ORDER[effort] : 99
}

/** 完整 slug 屬於哪一組（沒有就 null）。 */
export function agyGroupOf(groups: readonly AgyModelGroup[], id: string | null | undefined): AgyModelGroup | null {
  if (!id) return null
  return groups.find((g) => g.single?.id === id || [...g.variants.values()].some((v) => v.id === id)) ?? null
}

/** 完整 slug 的強度（沒有強度的群組回 null）。 */
export function agyEffortOf(group: AgyModelGroup | null, id: string | null | undefined): string | null {
  if (!group || !id) return null
  for (const [effort, v] of group.variants) if (v.id === id) return effort
  return null
}

/**
 * 切到另一個模型時要用的變體：保留目前強度（新模型有的話）→ medium → 第一個。
 * 沒有強度的群組直接回唯一的那顆。
 */
export function pickAgyVariant(group: AgyModelGroup, currentEffort: string | null): ModelInfo | null {
  if (group.single) return group.single
  if (currentEffort && group.variants.has(currentEffort)) return group.variants.get(currentEffort) ?? null
  if (group.variants.has('medium')) return group.variants.get('medium') ?? null
  const first = group.efforts[0]
  return first ? group.variants.get(first) ?? null : null
}

/** 群組內所有變體（沒有強度的群組就是 `single`）。 */
export function agyGroupMembers(group: AgyModelGroup): ModelInfo[] {
  return group.single ? [group.single] : [...group.variants.values()]
}
