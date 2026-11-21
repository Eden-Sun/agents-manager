
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

export function formatGptModelLabel(id: string): string {
  const parsed = parseGptModel(id)
  if (!parsed) return id
  const capitalizedFamily = parsed.family.charAt(0).toUpperCase() + parsed.family.slice(1)
  const suffix = parsed.suffix ? `-${parsed.suffix}` : ''
  return `${parsed.version}-${capitalizedFamily}${suffix}`
}

export function formatGptShortModel(id: string): string {
  const parsed = parseGptModel(id)
  if (!parsed) return id
  const suffix = parsed.suffix ? `-${parsed.suffix.toLowerCase()}` : ''
  return `${parsed.version}-${parsed.family}${suffix}`
}

export function compareVersions(av: readonly number[], bv: readonly number[]): number {
  for (let i = 0; i < Math.max(av.length, bv.length); i++) {
    const delta = (av[i] ?? -1) - (bv[i] ?? -1)
    if (delta !== 0) return delta
  }
  return 0
}

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

export function sortGptModelIds(ids: readonly string[]): string[] {
  return [...ids].sort(compareGptModels)
}
