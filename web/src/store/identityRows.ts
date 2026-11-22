import type { Bot, Identity, Project } from '../api/types'
import { isBuiltinDefaultIdentity } from '../lib/defaultIdentity'
import { t } from '../lib/i18n'

const LOCAL = 'local'

export function identityHost(i: Pick<Identity, 'host'>): string {
  return i.host || LOCAL
}

export function identityRowKey(i: Pick<Identity, 'host' | 'name'>): string {
  return `${identityHost(i)}:${i.name}`
}

export function findIdentity(list: readonly Identity[], host: string, name: string): Identity | undefined {
  return list.find((i) => i.name === name && identityHost(i) === (host || LOCAL))
}

export function effectiveIdentity(list: readonly Identity[], host: string, name: string): Identity | undefined {
  const want = host || LOCAL
  const exact = list.find((i) => i.name === name && identityHost(i) === want)
  if (exact) return exact
  if (want !== LOCAL) return list.find((i) => i.name === name && !i.host)
  return undefined
}

export function identitiesForHost(
  list: readonly Identity[],
  host: string,
  statuses?: Record<string, { source?: string }>,
): Identity[] {
  const want = host || LOCAL
  if (want === LOCAL) return list.filter((i) => identityHost(i) === LOCAL)
  const explicit = list.filter((i) => i.host === want)
  const taken = new Set(explicit.map((i) => i.name))
  const shared = list.filter(
    (i) => !i.host && !taken.has(i.name) && statuses?.[i.name]?.source !== 'shell',
  )
  return [...explicit, ...shared]
}

export function identityBots<B extends Pick<Bot, 'identity' | 'project_id'>>(
  bots: readonly B[],
  projects: readonly Pick<Project, 'id' | 'host'>[],
  host: string,
  name: string,
): B[] {
  const want = host || LOCAL
  return bots.filter((b) => b.identity === name && (projects.find((p) => p.id === b.project_id)?.host || LOCAL) === want)
}

export function identityUseCount(
  bots: readonly Pick<Bot, 'identity' | 'project_id'>[],
  projects: readonly Pick<Project, 'id' | 'host'>[],
  host: string,
  name: string,
): number {
  return identityBots(bots, projects, host, name).length
}

export function kindBots<B extends Pick<Bot, 'id' | 'name' | 'kind' | 'project_id'>>(
  bots: readonly B[],
  projects: readonly Pick<Project, 'id' | 'host'>[],
  host: string,
  kind: string,
): B[] {
  const want = host || LOCAL
  return bots.filter((b) => b.kind === kind && (projects.find((p) => p.id === b.project_id)?.host || LOCAL) === want)
}

export const DEFAULT_ACCOUNT = '@default'

export function defaultAccountBots<B extends Pick<Bot, 'id' | 'name' | 'identity' | 'kind' | 'project_id'>>(
  bots: readonly B[],
  projects: readonly Pick<Project, 'id' | 'host'>[],
  host: string,
  kind: string,
): B[] {
  return kindBots(bots, projects, host, kind).filter((b) => !b.identity || isBuiltinDefaultIdentity(kind, b.identity))
}

export function defaultAccountUseCount<B extends Pick<Bot, 'id' | 'name' | 'identity' | 'kind' | 'project_id'>>(
  bots: readonly B[],
  projects: readonly Pick<Project, 'id' | 'host'>[],
  host: string,
  kind: string,
): number {
  return defaultAccountBots(bots, projects, host, kind).length
}

export function shadowedByConfig(configured: readonly Identity[], host: string, name: string): boolean {
  return findIdentity(configured, host, name) !== undefined
}

export function hostLabel(host: string): string {
  return !host || host === LOCAL ? t('本機') : host
}
