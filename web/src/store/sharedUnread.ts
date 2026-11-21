import type { Bot } from '../api/types'
import type { ReadMark } from './unread'

export function laterMark(a: ReadMark | undefined | null, b: ReadMark | undefined | null): ReadMark | undefined {
  if (!a) return b ?? undefined
  if (!b) return a
  if (a.at !== b.at) return a.at > b.at ? a : b
  if (a.seq != null && b.seq != null && a.seq !== b.seq) return a.seq > b.seq ? a : b
  if (a.id === b.id && a.seq !== b.seq) return a.seq == null ? b : a
  return a.id >= b.id ? a : b
}

export function serverUnread(
  bots: readonly Bot[],
  current: Record<string, number>,
  viewing: (id: string) => boolean,
  unsent: { has: (id: string) => boolean } = new Set<string>(),
): Record<string, number> | null {
  let changed = false
  const next = { ...current }
  for (const b of bots) {
    if (b.pending || typeof b.unread !== 'number' || viewing(b.id) || unsent.has(b.id)) continue
    const have = current[b.id] ?? 0
    if (have === b.unread) continue
    changed = true
    if (b.unread > 0) next[b.id] = b.unread
    else delete next[b.id]
  }
  return changed ? next : null
}
