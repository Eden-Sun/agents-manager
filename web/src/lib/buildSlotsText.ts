import type { BuildSlots } from '../api/buildSlots'
import { fmtElapsed } from './elapsed'
import type { useT } from './i18n'

type T = ReturnType<typeof useT>

function since(iso: string, now: number): string {
  const at = Date.parse(iso)
  return Number.isNaN(at) ? '—' : fmtElapsed(Math.max(0, now - at))
}

export function buildCounts(s: BuildSlots): { active: number; waiting: number } {
  const localActive = s.slots.filter((x) => x.status === 'held').length
  const localWaiting = s.slots.filter((x) => x.status === 'waiting').length
  return { active: s.total_active ?? localActive, waiting: s.total_waiting ?? localWaiting }
}

export function buildTooltip(s: BuildSlots, botName: (id: string | null) => string, now: number, t: T): string {
  const { active, waiting } = buildCounts(s)
  const lines = [t('建置：{a} 個進行中、{w} 個排隊', { a: active, w: waiting })]
  const row = (where: string, purpose: string | null, bot: string | null, at: string, queued: boolean) =>
    `${queued ? t('排隊') : t('進行中')} · ${where} · ${purpose || '—'} · ${botName(bot)} · ${since(at, now)}`
  for (const x of s.slots) lines.push(row(t('本機'), x.purpose, x.bot_id, x.since, x.status !== 'held'))
  for (const r of s.remote_runs ?? []) lines.push(row(r.target === 'local' ? t('本機執行器') : (r.target ?? t('挑選建置主機中')), r.purpose, r.bot_id, r.since, r.target === null))
  for (const h of s.remote?.hosts ?? []) {
    const name = h.local ? t('本機執行器') : h.target
    lines.push(
      h.active == null
        ? t('{host}：讀不到名額', { host: name })
        : t('{host}：{a} 個進行中、{w} 個排隊', { host: name, a: h.active, w: h.waiting ?? 0 }),
    )
  }
  return lines.join('\n')
}
