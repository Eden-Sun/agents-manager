import { quotaKey } from '../api/types'
import { isBuiltinDefaultIdentity } from '../lib/defaultIdentity'
import type { BotKind, Identity, KindQuota, QuotaMap } from '../api/types'
import { projectNoFiveHour } from '../lib/quotaNoFiveHour'

export function weeklyOnlyKind(kind: BotKind): boolean {
  return kind === 'grok'
}

export const DEFAULT_IDENTITY = 'cc0'

const HOME_VAR: Partial<Record<BotKind, string>> = { claude: 'CLAUDE_CONFIG_DIR', codex: 'CODEX_HOME', grok: 'GROK_HOME', agy: 'HOME' }

export function sharesBareQuotaKey(idn: Identity): boolean {
  const v = HOME_VAR[idn.kind]
  return v ? !(v in idn.env) : Object.keys(idn.env).length === 0
}

export function bareQuotaOwner(identities: readonly Identity[], kind: BotKind): string | null {
  const ofKind = identities.filter((i) => i.kind === kind)
  if (ofKind.length === 0) return DEFAULT_IDENTITY
  if (ofKind.some((i) => i.name === DEFAULT_IDENTITY && sharesBareQuotaKey(i))) return DEFAULT_IDENTITY
  const sharing = ofKind.filter(sharesBareQuotaKey).map((i) => i.name).sort()
  return sharing[0] ?? null
}

export function quotaBaseKey(
  quota: QuotaMap,
  host: string,
  kind: BotKind,
  identity: string | null,
  identities: readonly Identity[],
): string {
  if (!identity || isBuiltinDefaultIdentity(kind, identity)) return kind
  const own = identities.find((i) => i.name === identity)
  if (own && own.kind !== kind) return kind
  const keyed = `${kind}:${identity}`
  if (own) return bareQuotaOwner(identities, kind) === identity ? kind : keyed
  if (quota[quotaKey(host, keyed)] != null) return keyed
  const bare = quotaKey(host, kind)
  return bareQuotaOwner(identities, kind) === identity && bare in quota ? kind : keyed
}

export function quotaForIdentity(
  quota: QuotaMap,
  host: string,
  kind: BotKind,
  identity: string | null,
  identities: readonly Identity[],
): KindQuota | null {
  return projectNoFiveHour(kind, quota[quotaKey(host, quotaBaseKey(quota, host, kind, identity, identities))] ?? null)
}
