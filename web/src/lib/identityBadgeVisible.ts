import type { BotKind, Identity } from '../api/types'

export function identityBadgeVisible(
  kind: BotKind,
  hostIdentities: readonly Pick<Identity, 'name' | 'kind'>[],
  usedNames: readonly (string | null | undefined)[] = [],
): boolean {
  const names = new Set<string>()
  for (const i of hostIdentities) if (i.kind === kind) names.add(i.name)
  for (const n of usedNames) if (n) names.add(n)
  const choices = names.size + (names.size > 0 ? 1 : 0)
  return choices >= 2
}
