import type { ModelInfo } from '../api/types'
import { AGY_EFFORT_ORDER, formatAgyModelLabel, isAgyOrchestrator, parseAgyModel } from './agyModel'

export interface AgyModelGroup {
  key: string
  label: string
  efforts: string[]
  variants: Map<string, ModelInfo>
  single: ModelInfo | null
  isDefault: boolean
}

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

export function agyGroupOf(groups: readonly AgyModelGroup[], id: string | null | undefined): AgyModelGroup | null {
  if (!id) return null
  return groups.find((g) => g.single?.id === id || [...g.variants.values()].some((v) => v.id === id)) ?? null
}

export function agyEffortOf(group: AgyModelGroup | null, id: string | null | undefined): string | null {
  if (!group || !id) return null
  for (const [effort, v] of group.variants) if (v.id === id) return effort
  return null
}

export function pickAgyVariant(group: AgyModelGroup, currentEffort: string | null): ModelInfo | null {
  if (group.single) return group.single
  if (currentEffort && group.variants.has(currentEffort)) return group.variants.get(currentEffort) ?? null
  if (group.variants.has('medium')) return group.variants.get('medium') ?? null
  const first = group.efforts[0]
  return first ? group.variants.get(first) ?? null : null
}

export function agyGroupMembers(group: AgyModelGroup): ModelInfo[] {
  return group.single ? [group.single] : [...group.variants.values()]
}
