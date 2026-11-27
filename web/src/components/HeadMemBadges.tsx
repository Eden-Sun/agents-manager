import type { ReactNode } from 'react'
import { useShallow } from 'zustand/react/shallow'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { LOCAL_HOST, type HostMem } from '../api/types'
import { BuildSlotsChip } from './BuildSlotsChip'
import { MemBadge } from './MemBadge'
import { MemPopover } from './MemPopover'
import { humanBytes } from './memFormat'

type HostSummary = {
  host: string
  total: number
  free: number | null
  freePct: number | null
  cpu: number | null
  low: boolean
  cpuHot: boolean
  error: boolean
}

function summarize(row: HostMem): HostSummary {
  const m = row.machine
  const freePct = m && m.total_bytes > 0 ? (m.available_bytes / m.total_bytes) * 100 : null
  const cpu = m && typeof m.cpu_pct === 'number' && Number.isFinite(m.cpu_pct) ? Math.round(m.cpu_pct) : null
  return {
    host: row.host,
    total: row.total_bytes,
    free: m ? m.available_bytes : null,
    freePct,
    cpu,
    low: freePct !== null && freePct < 15,
    cpuHot: cpu !== null && cpu >= 85,
    error: Boolean(row.error),
  }
}

function pickFocus(rows: HostSummary[]): HostSummary | undefined {
  return rows.find((r) => r.low || r.cpuHot) ?? rows.find((r) => r.host === LOCAL_HOST) ?? rows[0]
}

function HostsSummary({ hosts }: { hosts: HostMem[] }) {
  const t = useT()
  const rows = hosts.map(summarize)
  const focus = pickFocus(rows)
  if (!focus) return null
  const alert = focus.low || focus.cpuHot
  const name = (h: string) => (h === LOCAL_HOST ? t('本機') : `@${h}`)
  const line = (r: HostSummary) =>
    r.error
      ? t('{host}：量不到', { host: name(r.host) })
      : [
          t('{host}：RAM {ram}', { host: name(r.host), ram: humanBytes(r.total) }),
          r.free !== null ? t('剩 {bytes}（{pct}%）', { bytes: humanBytes(r.free), pct: Math.round(r.freePct ?? 0) }) : '',
          r.cpu !== null ? t('CPU {pct}%', { pct: r.cpu }) : '',
          r.low || r.cpuHot ? '⚠' : '',
        ]
          .filter(Boolean)
          .join(' · ')
  const tip = [...rows.map(line), '', t('點一下看各主機的程序（可切換主機）。')].join('\n')
  const value = focus.cpuHot ? `${focus.cpu}%` : focus.low ? t('剩 {pct}%', { pct: Math.round(focus.freePct ?? 0) }) : null
  return (
    <MemPopover host={focus.host} hosts={rows.map((r) => r.host)}>
      <span className={`mem-badge head-hosts${alert ? ' alert' : ''}`} data-k="head-hosts" title={tip}>
        {alert ? (
          <>
            <span className="mem-k">{t('主機 {n}', { n: rows.length })}</span>
            <span className="head-hosts-sep" aria-hidden="true">·</span>
            <span className="head-hosts-alert">
              {name(focus.host)} {value}
            </span>
          </>
        ) : (
          <>
            <span className="mem-k">{name(focus.host)}</span>
            <span className="mem-v">{humanBytes(focus.total)}</span>
            {focus.cpu !== null ? <span className="mem-cpu">·{focus.cpu}%</span> : null}
            <span className="head-hosts-more">+{rows.length - 1}</span>
          </>
        )}
        <span className="head-hosts-caret" aria-hidden="true">▾</span>
      </span>
    </MemPopover>
  )
}

export function HeadMemBadges({ pane }: { pane?: ReactNode }) {
  const hosts = useStore(useShallow((s) => s.mem?.hosts ?? []))
  const multi = hosts.filter((h) => h.host !== LOCAL_HOST).length > 0
  return (
    <div className={`head-ram${multi ? ' multi' : ''}`}>
      {pane}
      <BuildSlotsChip />
      {multi ? <HostsSummary hosts={hosts} /> : <MemBadge />}
    </div>
  )
}
