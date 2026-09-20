import type { BotKind } from '../api/types'

export function hostLabel(host: string): string {
  return host === 'local' ? '本機' : host
}

/** Missing tools that affect the current bot or group members. */
export function relevantMissing(
  all: { host: string; kind: BotKind }[],
  focusHost: string | null | undefined,
  focusKinds: BotKind[] | null | undefined,
): { host: string; kind: BotKind }[] {
  if (!focusHost || !focusKinds || focusKinds.length === 0) return []
  const want = new Set(focusKinds)
  return all.filter((m) => m.host === focusHost && want.has(m.kind))
}
