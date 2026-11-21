
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

const ORCHESTRATOR_FAMILY_ALIAS: Record<string, string> = {
  'g1-pro': 'argon',
}

export function isAgyOrchestrator(id: string): boolean {
  return /^gemini-pro-orchestrator-[a-z0-9-]+$/.test(id)
}

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

export function formatAgyEffortLabel(effort: string): string {
  if (effort.toLowerCase() === 'xhigh') return 'XHigh'
  return effort.charAt(0).toUpperCase() + effort.slice(1).toLowerCase()
}

export function formatAgyModelLabel(id: string): string {
  if (isAgyOrchestrator(id)) {
    const raw = id.slice('gemini-pro-orchestrator-'.length)
    const family = ORCHESTRATOR_FAMILY_ALIAS[raw] ?? raw
    return `Orchestrator ${family.charAt(0).toUpperCase()}${family.slice(1)}`
  }
  const parsed = parseAgyModel(id)
  if (!parsed) return id
  const capitalizedFamily = parsed.family.charAt(0).toUpperCase() + parsed.family.slice(1)
  const effortPart = parsed.effort ? ` ${formatAgyEffortLabel(parsed.effort)}` : ''
  return `${parsed.version}-${capitalizedFamily}${effortPart}`
}

export function compareVersions(av: readonly number[], bv: readonly number[]): number {
  for (let i = 0; i < Math.max(av.length, bv.length); i++) {
    const delta = (av[i] ?? -1) - (bv[i] ?? -1)
    if (delta !== 0) return delta
  }
  return 0
}

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
    if (pa.familyRank === 99 && pa.family !== pb.family) {
      return pa.family.localeCompare(pb.family)
    }
    const ver = compareVersions(pa.versionNumbers, pb.versionNumbers)
    if (ver !== 0) return ver
    if (pa.effortRank !== pb.effortRank) {
      return pa.effortRank - pb.effortRank
    }
    return 0
  }
  if (pa && !pb) return -1
  if (!pa && pb) return 1
  const oa = isAgyOrchestrator(aId)
  const ob = isAgyOrchestrator(bId)
  if (oa && !ob) return -1
  if (!oa && ob) return 1
  return 0
}

export function sortAgyModelIds(ids: readonly string[]): string[] {
  const familyOrder = buildAgyFamilyOrder(ids)
  return ids
    .map((id, i) => ({ id, i }))
    .sort((a, b) => compareAgyModels(a.id, b.id, familyOrder) || a.i - b.i)
    .map((x) => x.id)
}
