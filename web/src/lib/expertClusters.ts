/** 同群集的成員（cluster 相同、≥2 顆）收成一列；其餘維持單列。保留原順序，群集列落在第一個成員的位置。 */
export type ExpertClusterItem<T extends { cluster?: string | null }> =
  | { kind: 'single'; item: T }
  | { kind: 'cluster'; name: string; members: T[] }

export function groupClusterItems<T extends { cluster?: string | null }>(items: readonly T[]): ExpertClusterItem<T>[] {
  const count = new Map<string, number>()
  for (const b of items) if (b.cluster) count.set(b.cluster, (count.get(b.cluster) ?? 0) + 1)
  const out: ExpertClusterItem<T>[] = []
  const seen = new Map<string, T[]>()
  for (const b of items) {
    const name = b.cluster
    if (!name || (count.get(name) ?? 0) < 2) {
      out.push({ kind: 'single', item: b })
      continue
    }
    const list = seen.get(name)
    if (list) {
      list.push(b)
    } else {
      const members = [b]
      seen.set(name, members)
      out.push({ kind: 'cluster', name, members })
    }
  }
  return out
}
