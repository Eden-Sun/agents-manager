
export const MESSAGE_CAP = 500

export const TURN_CAP = 50

function bySeqThenId(a: { id: string; seq?: number }, b: { id: string; seq?: number }): number {
  if (a.seq && b.seq && a.seq !== b.seq) return a.seq < b.seq ? -1 : 1
  return a.id.localeCompare(b.id)
}

export function byTime(a: { created_at: string; id: string; seq?: number }, b: { created_at: string; id: string; seq?: number }): number {
  const t = a.created_at.localeCompare(b.created_at)
  return t !== 0 ? t : bySeqThenId(a, b)
}

export function byInsert(a: { id: string; seq?: number }, b: { id: string; seq?: number }): number {
  return bySeqThenId(a, b)
}

export function oldestByInsert<T extends { id: string; seq?: number }>(items: readonly T[]): T | undefined {
  let oldest: T | undefined
  for (const item of items) if (!oldest || byInsert(item, oldest) < 0) oldest = item
  return oldest
}

export function insertSorted<T extends { id: string }>(
  list: readonly T[],
  item: T,
  cmp: (a: T, b: T) => number,
): T[] | null {
  let at = list.length
  while (at > 0) {
    const prev = list[at - 1]
    if (prev.id === item.id) return null
    if (cmp(prev, item) <= 0) break
    at--
  }
  if (at === list.length) return [...list, item]
  return [...list.slice(0, at), item, ...list.slice(at)]
}

export function upsertSorted<T extends { id: string }>(
  list: readonly T[],
  item: T,
  cmp: (a: T, b: T) => number,
  same: (a: T, b: T) => boolean,
): T[] | null {
  const at = list.findIndex((x) => x.id === item.id)
  if (at < 0) return insertSorted(list, item, cmp)
  if (same(list[at], item)) return null
  return insertSorted([...list.slice(0, at), ...list.slice(at + 1)], item, cmp)
}

export function capList<T>(list: T[], cap: number): { list: T[]; trimmed: boolean } {
  if (list.length <= cap) return { list, trimmed: false }
  return { list: list.slice(list.length - cap), trimmed: true }
}

export interface Turnish {
  status: string
  delivery?: string | null
}

export function pruneTurns<T extends Turnish>(map: Record<string, T>, cap = TURN_CAP): Record<string, T> {
  const ids = Object.keys(map)
  if (ids.length <= cap) return map
  const keep = new Set(ids.sort((a, b) => a.localeCompare(b)).slice(-cap))
  for (const id of ids) {
    const t = map[id]
    if (t.status === 'in_flight') keep.add(id)
  }
  if (keep.size === ids.length) return map
  const out: Record<string, T> = {}
  for (const id of ids) if (keep.has(id)) out[id] = map[id]
  return out
}

export function keptAfterPage<T extends { id: string; created_at: string; seq?: number }>(existing: T[], page: T[], startedAt: string): T[] {
  let newest: T | null = null
  for (const m of page) if (!newest || byTime(m, newest) > 0) newest = m
  const seen = new Set(page.map((m) => m.id))
  return existing.filter((m) => !seen.has(m.id) && (newest ? byTime(m, newest) > 0 : m.created_at > startedAt))
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false
  if (Array.isArray(a) !== Array.isArray(b)) return false
  const ka = Object.keys(a)
  if (ka.length !== Object.keys(b).length) return false
  for (const k of ka) {
    if (!deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k])) return false
  }
  return true
}

export function reuseUnchanged<T extends { id: string }>(prev: readonly T[], next: T[]): T[] {
  if (prev.length === 0) return next
  const old = new Map<string, T>()
  for (const x of prev) old.set(x.id, x)
  let same = prev.length === next.length
  const out = next.map((n, i) => {
    const o = old.get(n.id)
    if (o !== undefined && deepEqual(o, n)) {
      if (prev[i] !== o) same = false
      return o
    }
    same = false
    return n
  })
  return same ? (prev as T[]) : out
}
