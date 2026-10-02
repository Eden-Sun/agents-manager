import type { Bot, Host, Project } from '../api/types'

/** 一台離線的遠端主機，連同它拖下水的 bot（離線警示條、聊天區橫幅共用）。 */
export interface OfflineHost {
  name: string
  error: string | null
  /** daemon 的 `disconnected_since`；舊 daemon 沒這欄＝null，就不寫「離線多久」。 */
  since: string | null
  bots: { id: string; name: string; project: string }[]
}

type Input = { hosts: readonly Host[]; projects: readonly Project[]; bots: readonly Bot[] }

/**
 * 有專案掛著的離線遠端主機（設定了但沒有專案用到的不算：沒東西受影響，常駐紅條只是雜訊，
 * 狀態在「環境設定 → 主機」看得到）。專案指到 daemon 沒回報的主機也算離線（同 `botLamp`）。
 */
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

/** 這顆 bot 所在的遠端主機離線時回主機名，否則 null（本機 herdr 斷線另有 ConnBanner，不算這裡）。 */
export function botOfflineHost({ hosts, projects, bots }: Input, botId: string | null): string | null {
  const bot = bots.find((b) => b.id === botId)
  const host = projects.find((p) => p.id === bot?.project_id)?.host
  if (!host || host === 'local') return null
  return hosts.find((h) => h.name === host)?.connected ? null : host
}

/** 「離線多久」：不到一分鐘「剛剛」，之後 N 分鐘／N 小時 M 分／N 天 M 小時。時間讀不懂回 null。 */
export function downFor(since: string | null, now: number): string | null {
  if (!since) return null
  const t = Date.parse(since)
  if (Number.isNaN(t)) return null
  const m = Math.max(0, Math.floor((now - t) / 60_000))
  if (m < 1) return '剛剛'
  if (m < 60) return `${m} 分鐘`
  const h = Math.floor(m / 60)
  if (h < 24) return m % 60 ? `${h} 小時 ${m % 60} 分` : `${h} 小時`
  const d = Math.floor(h / 24)
  return h % 24 ? `${d} 天 ${h % 24} 小時` : `${d} 天`
}
