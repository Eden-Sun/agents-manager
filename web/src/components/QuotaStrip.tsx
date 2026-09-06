import { useEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, KindQuota, QuotaWindow } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { useStore } from '../store/store'
import { KindIcon, KIND_LABEL } from './KindTag'

/**
 * Remaining quota per kind (`GET /api/quota` + WS `quota_updated`).
 *
 * Decision (Codex sol, docs/UI-DECISIONS.md follow-up): three equal always-on pills were
 * wrong — the kinds do not have the same data capability. grok's CLI cannot report quota at
 * all, so it never takes strip space and is explained in the popover instead. The strip
 * keeps every queryable kind (claude, codex) permanently visible; only the *windows* per kind
 * collapse below 1100px, never a whole kind. Drawn as frameless health bars: the fill length
 * carries the level, colour only reinforces it, so it survives greyscale. Full numbers live in
 * the tooltip and popover.
 */

/** Only these can ever report quota; grok has no such CLI surface. */
const QUERYABLE: BotKind[] = ['claude', 'codex']

type Level = 'crit' | 'warn' | 'ok'

function remaining(w: QuotaWindow | null | undefined): number | null {
  return w ? Math.max(0, Math.round(100 - w.used_pct)) : null
}

function levelOf(pct: number | null): Level {
  if (pct === null) return 'ok'
  if (pct < 10) return 'crit'
  if (pct < 30) return 'warn'
  return 'ok'
}

function worst(q: KindQuota | null): { pct: number | null; level: Level } {
  const vals = [remaining(q?.five_hour), remaining(q?.seven_day)].filter((n): n is number => n !== null)
  if (!vals.length) return { pct: null, level: 'ok' }
  const pct = Math.min(...vals)
  return { pct, level: levelOf(pct) }
}

/** The window that is closest to running out — what the collapsed pill shows. */
function worstWindow(q: KindQuota | null): { name: '5h' | '7d'; pct: number | null } {
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  if (five === null && seven === null) return { name: '5h', pct: null }
  if (five === null) return { name: '7d', pct: seven }
  if (seven === null) return { name: '5h', pct: five }
  return five <= seven ? { name: '5h', pct: five } : { name: '7d', pct: seven }
}

function fmtTime(iso: string | null | undefined): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '—'
  return d.toLocaleString([], { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
}

/** `5h 81%` / `5h —`; never wraps, always the same shape. */
function pctText(pct: number | null): string {
  return pct === null ? '—' : `${pct}%`
}

function label(kind: BotKind, q: KindQuota | null): string {
  const parts = [KIND_LABEL[kind]]
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  parts.push(five === null ? '5 小時額度無資料' : `5 小時剩餘 ${five}%`)
  parts.push(seven === null ? '7 天額度無資料' : `7 天剩餘 ${seven}%`)
  if (q?.five_hour?.resets_at) parts.push(`5 小時 ${fmtTime(q.five_hour.resets_at)} 重置`)
  if (q?.seven_day?.resets_at) parts.push(`7 天 ${fmtTime(q.seven_day.resets_at)} 重置`)
  return parts.join('，')
}

/** Shape + colour, so the level survives greyscale and colour blindness. */
function RiskDot({ level }: { level: Level }) {
  return <span className={`quota-risk ${level}`} aria-hidden="true" />
}

/** One window as a health bar: length carries the level, colour only reinforces it. */
function Bar({ pct }: { pct: number | null }) {
  const lv = levelOf(pct)
  return (
    <span className={`quota-bar ${lv}${pct === null ? ' nodata' : ''}`}>
      <span className="quota-bar-fill" style={{ width: pct === null ? '0%' : `${pct}%` }} />
    </span>
  )
}

/** Frameless, compact: kind glyph + one bar per window (collapsed shows the worst one). */
function Gauge({ kind, collapsed }: { kind: BotKind; collapsed: boolean }) {
  const q = useStore((s) => s.quota[kind] ?? null)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const w = worstWindow(q)

  return (
    <span className={`quota-hp ${kind} ${worst(q).level}`} title={label(kind, q)} aria-label={label(kind, q)}>
      <span className="quota-kind" aria-hidden="true">
        <KindIcon kind={kind} />
      </span>
      <span className="quota-bars">
        {collapsed ? <Bar pct={w.pct} /> : <><Bar pct={five} /><Bar pct={seven} /></>}
      </span>
    </span>
  )
}

function PopRow({ kind }: { kind: BotKind }) {
  const q = useStore((s) => s.quota[kind] ?? null)
  const known = useStore((s) => kind in s.quota)
  const supported = QUERYABLE.includes(kind)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)

  return (
    <div className="quota-pop-row">
      <div className="quota-pop-head">
        <span className="quota-kind" aria-hidden="true">
          <KindIcon kind={kind} />
        </span>
        <span className="quota-name">{KIND_LABEL[kind]}</span>
        {supported ? <RiskDot level={worst(q).level} /> : null}
        {q?.plan ? <span className="quota-plan">{q.plan}</span> : null}
      </div>
      {!supported ? (
        <p className="quota-pop-note">CLI 不支援額度查詢</p>
      ) : !known ? (
        <p className="quota-pop-note">尚未取得（啟動一個 {KIND_LABEL[kind]} bot 後回報）</p>
      ) : (
        <>
          <div className="quota-pop-line">
            <span>5 小時</span>
            <span className={`quota-row ${levelOf(five)}`}>剩 {pctText(five)}</span>
            <span className="quota-reset">{fmtTime(q?.five_hour?.resets_at)} 重置</span>
          </div>
          <div className="quota-pop-line">
            <span>7 天</span>
            <span className={`quota-row ${levelOf(seven)}`}>剩 {pctText(seven)}</span>
            <span className="quota-reset">{fmtTime(q?.seven_day?.resets_at)} 重置</span>
          </div>
          {q?.updated_at ? <p className="quota-pop-note">更新於 {fmtTime(q.updated_at)}</p> : null}
        </>
      )}
    </div>
  )
}

export function QuotaStrip({ focusKind }: { focusKind?: BotKind | null }) {
  const quota = useStore((s) => s.quota)
  const [width, setWidth] = useState(() => (typeof window !== 'undefined' ? window.innerWidth : 1440))
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const onResize = () => setWidth(window.innerWidth)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [])

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  /** Critical first, then warning, then by remaining; the focused kind wins a tie. */
  const ordered = useMemo(() => {
    const rank: Record<Level, number> = { crit: 0, warn: 1, ok: 2 }
    return QUERYABLE.filter((k) => k in quota)
      .map((k) => ({ kind: k, ...worst(quota[k] ?? null) }))
      .sort((a, b) => {
        if (rank[a.level] !== rank[b.level]) return rank[a.level] - rank[b.level]
        const av = a.pct ?? 101
        const bv = b.pct ?? 101
        if (av !== bv) return av - bv
        if (a.kind === focusKind) return -1
        if (b.kind === focusKind) return 1
        return 0
      })
      .map((x) => x.kind)
  }, [quota, focusKind])

  if (ordered.length === 0) return null

  // Both queryable kinds always stay on the bar; only the per-kind windows collapse.
  const collapsed = width < 1100

  return (
    <div className="quota-strip" ref={wrap} aria-label="額度">
      <button
        type="button"
        className={`quota-open${collapsed ? ' collapsed' : ''}`}
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-label="所有額度"
        title="所有額度"
        onClick={() => setOpen((v) => !v)}
      >
        {ordered.map((k) => (
          <Gauge key={k} kind={k} collapsed={collapsed} />
        ))}
      </button>
      {open ? (
        <div className="quota-pop" role="dialog" aria-label="所有額度">
          {BOT_KINDS.map((k) => (
            <PopRow key={k} kind={k} />
          ))}
        </div>
      ) : null}
    </div>
  )
}
