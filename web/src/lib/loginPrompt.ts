import type { Bot, Host, IdentityStatusMap, Project } from '../api/types'
import { t } from './i18n'

export interface PendingLogin {
  host: string
  identity: string
  episode: string
  via: 'probe' | 'turn'
  bots: { id: string; name: string }[]
}

export interface LoginPromptInput {
  bots: Pick<Bot, 'id' | 'name' | 'project_id' | 'identity' | 'kind'>[]
  projects: Pick<Project, 'id' | 'host'>[]
  hosts: Pick<Host, 'name' | 'connected' | 'identity_status'>[]
  localIdentityStatus: IdentityStatusMap
  localConnected: boolean
}

export const dismissKey = (host: string, identity: string) => `${host}/${identity}`

export function pendingLogins(s: LoginPromptInput): PendingLogin[] {
  const hostOf = new Map(s.projects.map((p) => [p.id, p.host || 'local']))
  const sources: { host: string; status: IdentityStatusMap }[] = [
    ...(s.localConnected ? [{ host: 'local', status: s.localIdentityStatus }] : []),
    ...s.hosts.filter((h) => h.connected).map((h) => ({ host: h.name, status: h.identity_status })),
  ]
  const out: PendingLogin[] = []
  for (const { host, status } of sources) {
    for (const [identity, st] of Object.entries(status)) {
      if (st.kind !== 'claude' && st.kind !== 'codex') continue
      const turn = st.login_needed ?? null
      if (st.logged_in !== false && !turn) continue
      const bots = s.bots
        .filter((b) => b.kind === st.kind && b.identity === identity && (hostOf.get(b.project_id) ?? 'local') === host)
        .map((b) => ({ id: b.id, name: b.name }))
      if (bots.length === 0) continue
      out.push({ host, identity, episode: turn?.since ?? 'probe', via: turn ? 'turn' : 'probe', bots })
    }
  }
  return out
}

export function visibleLogins(pending: PendingLogin[], dismissed: Record<string, string>): PendingLogin[] {
  return pending.filter((p) => dismissed[dismissKey(p.host, p.identity)] !== p.episode)
}

export function pruneDismissed(dismissed: Record<string, string>, pending: PendingLogin[]): Record<string, string> {
  const live = new Set(pending.map((p) => dismissKey(p.host, p.identity)))
  const keys = Object.keys(dismissed)
  if (keys.every((k) => live.has(k))) return dismissed
  return Object.fromEntries(keys.filter((k) => live.has(k)).map((k) => [k, dismissed[k]]))
}

export function pendingForPrune(
  s: Pick<LoginPromptInput, 'hosts' | 'localConnected'>,
  pending: PendingLogin[],
  dismissed: Record<string, string>,
): PendingLogin[] {
  const live = new Set([...(s.localConnected ? ['local'] : []), ...s.hosts.filter((h) => h.connected).map((h) => h.name)])
  const keep: PendingLogin[] = []
  for (const [k, episode] of Object.entries(dismissed)) {
    const at = k.indexOf('/')
    const host = k.slice(0, at)
    if (at > 0 && !live.has(host)) keep.push({ host, identity: k.slice(at + 1), episode, via: 'probe', bots: [] })
  }
  return keep.length ? [...pending, ...keep] : pending
}

export function loginPromptText(p: PendingLogin): { title: string; detail: string } {
  const where = p.host === 'local' ? t('本機') : p.host
  const n = p.bots.length
  return {
    title: t('{identity}（{where}）已登出', { identity: p.identity, where }),
    detail: p.via === 'turn' ? t('{n} 顆 bot 在用它，剛才有回合因為登入失效失敗', { n }) : t('偵測到已登出，{n} 顆 bot 在用它', { n }),
  }
}
