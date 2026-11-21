import type { Bot, Host, Project } from '../api/types'
import { t } from './i18n'

export interface OfflineHost {
  name: string
  error: string | null
  since: string | null
  bots: { id: string; name: string; project: string }[]
}

type Input = { hosts: readonly Host[]; projects: readonly Project[]; bots: readonly Bot[] }

export function offlineHosts({ hosts, projects, bots }: Input): OfflineHost[] {
  const out = new Map<string, OfflineHost>()
  for (const p of projects) {
    if (!p.host || p.host === 'local') continue
    const h = hosts.find((x) => x.name === p.host)
    if (h?.connected) continue
    let row = out.get(p.host)
    if (!row) {
      row = { name: p.host, error: h?.error ?? null, since: h?.disconnected_since ?? null, bots: [] }
      out.set(p.host, row)
    }
    for (const b of bots) {
      if (b.project_id === p.id && !b.pending) row.bots.push({ id: b.id, name: b.name, project: p.label })
    }
  }
  return [...out.values()].sort((a, b) => a.name.localeCompare(b.name))
}

export function botOfflineHost({ hosts, projects, bots }: Input, botId: string | null): string | null {
  const bot = bots.find((b) => b.id === botId)
  const host = projects.find((p) => p.id === bot?.project_id)?.host
  if (!host || host === 'local') return null
  return hosts.find((h) => h.name === host)?.connected ? null : host
}

export function downFor(since: string | null, now: number): string | null {
  if (!since) return null
  const ms = Date.parse(since)
  if (Number.isNaN(ms)) return null
  const m = Math.max(0, Math.floor((now - ms) / 60_000))
  if (m < 1) return t('剛剛')
  if (m < 60) return t('{n} 分鐘', { n: m })
  const h = Math.floor(m / 60)
  if (h < 24) return m % 60 ? t('{n} 小時 {m} 分', { n: h, m: m % 60 }) : t('{n} 小時', { n: h })
  const d = Math.floor(h / 24)
  return h % 24 ? t('{n} 天 {h} 小時', { n: d, h: h % 24 }) : t('{n} 天', { n: d })
}
