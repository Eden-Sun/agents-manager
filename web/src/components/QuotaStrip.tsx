import { useEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, KindQuota, QuotaMap, QuotaWindow } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { useStore } from '../store/store'
import { KindIcon, KIND_LABEL } from './KindTag'

/**
 * Remaining quota per kind (`GET /api/quota` + WS `quota_updated`).
 *
 * Decision (Codex sol, docs/UI-DECISIONS.md follow-up): the strip keeps every kind that has
 * reported quota permanently visible; only the *windows* per kind collapse below 1100px, never
 * a whole kind. Drawn as frameless health bars: the fill length carries the level, colour only
 * reinforces it, so it survives greyscale. Full numbers live in the tooltip and popover.
 *
 * The kinds do not report the same windows: claude and codex have both 5h and 7d, grok only a
 * weekly one (scraped from its `/usage` dialog, SPEC §12.6). A kind therefore draws one bar per
 * window it actually reports — never a filler bar for a window that does not exist.
 *
 * Claude may also report per-identity keys (`claude:cc1`, …). Each such key gets its own gauge
 * (same icon + small identity label), so cc0/default and cc1 both show when both exist.
 */

/** Every kind can report quota; grok arrives from the `/usage` probe. */
const QUERYABLE: BotKind[] = ['claude', 'codex', 'grok']

/** One strip / popover row: base kind or `kind:identity`. */
type QuotaEntry = { key: string; kind: BotKind; identity: string | null }

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

function entryLabel(entry: QuotaEntry): string {
  return entry.identity ? `${KIND_LABEL[entry.kind]} · ${entry.identity}` : KIND_LABEL[entry.kind]
}

function label(entry: QuotaEntry, q: KindQuota | null): string {
  const parts = [entryLabel(entry)]
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  if (five === null && seven === null) parts.push('額度尚未取得')
  if (five !== null) parts.push(`5 小時剩餘 ${five}%`)
  if (seven !== null) parts.push(`7 天剩餘 ${seven}%`)
  if (q?.five_hour?.resets_at) parts.push(`5 小時 ${fmtTime(q.five_hour.resets_at)} 重置`)
  if (q?.seven_day?.resets_at) parts.push(`7 天 ${fmtTime(q.seven_day.resets_at)} 重置`)
  return parts.join('，')
}

/** Parse `claude` / `claude:cc1` into a strip entry; unknown kinds are ignored. */
function parseQuotaKey(key: string): QuotaEntry | null {
  const i = key.indexOf(':')
  const kind = (i === -1 ? key : key.slice(0, i)) as BotKind
  if (!QUERYABLE.includes(kind)) return null
  const identity = i === -1 ? null : key.slice(i + 1) || null
  if (identity !== null && !identity) return null
  return { key, kind, identity }
}

/**
 * Keys to draw: every base kind present in the map, plus each `kind:identity` that has its own
 * row. Base kinds with a null placeholder (daemon always emits them) still count as "present"
 * so the strip stays stable; identity keys only appear once they have reported.
 */
function collectEntries(quota: QuotaMap): QuotaEntry[] {
  const out: QuotaEntry[] = []
  const seen = new Set<string>()
  for (const kind of QUERYABLE) {
    if (kind in quota) {
      out.push({ key: kind, kind, identity: null })
      seen.add(kind)
    }
  }
  for (const key of Object.keys(quota)) {
    if (seen.has(key)) continue
    const entry = parseQuotaKey(key)
    if (!entry || !entry.identity) continue
    if (quota[key] == null) continue
    out.push(entry)
    seen.add(key)
  }
  return out
}

function kindRank(kind: BotKind): number {
  const i = QUERYABLE.indexOf(kind)
  return i === -1 ? QUERYABLE.length : i
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

/** Frameless, compact: kind glyph (+ identity) + one bar per reported window (collapsed = worst). */
function Gauge({ entry, collapsed }: { entry: QuotaEntry; collapsed: boolean }) {
  const q = useStore((s) => s.quota[entry.key] ?? null)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  // Only windows this kind actually reports; a lone hatched bar when it reports none yet.
  const present = [five, seven].filter((v): v is number => v !== null)
  const bars = collapsed ? [worstWindow(q).pct] : present.length ? present : [null]
  const title = label(entry, q)

  return (
    <span className={`quota-hp ${entry.kind} ${worst(q).level}`} title={title} aria-label={title}>
      <span className="quota-kind" aria-hidden="true">
        <KindIcon kind={entry.kind} />
      </span>
      {entry.identity ? (
        <span className="quota-identity" aria-hidden="true">
          {entry.identity}
        </span>
      ) : null}
      <span className="quota-bars">
        {bars.map((pct, i) => (
          <Bar key={i} pct={pct} />
        ))}
      </span>
    </span>
  )
}

function PopRow({ entry }: { entry: QuotaEntry }) {
  const q = useStore((s) => s.quota[entry.key] ?? null)
  const known = useStore((s) => entry.key in s.quota)
  const supported = QUERYABLE.includes(entry.kind)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)

  return (
    <div className="quota-pop-row">
      <div className="quota-pop-head">
        <span className="quota-kind" aria-hidden="true">
          <KindIcon kind={entry.kind} />
        </span>
        <span className="quota-name">{entryLabel(entry)}</span>
        {supported ? <RiskDot level={worst(q).level} /> : null}
        {q?.plan ? <span className="quota-plan">{q.plan}</span> : null}
      </div>
      {!supported ? (
        <p className="quota-pop-note">CLI 不支援額度查詢</p>
      ) : !known || (five === null && seven === null) ? (
        <p className="quota-pop-note">
          {entry.kind === 'grok' ? '背景查詢中' : `尚未取得（啟動一個 ${KIND_LABEL[entry.kind]} bot 後回報）`}
        </p>
      ) : (
        <>
          {five !== null ? (
            <div className="quota-pop-line">
              <span>5 小時</span>
              <span className={`quota-row ${levelOf(five)}`}>剩 {pctText(five)}</span>
              <span className="quota-reset">{fmtTime(q?.five_hour?.resets_at)} 重置</span>
            </div>
          ) : null}
          {seven !== null ? (
            <div className="quota-pop-line">
              <span>{entry.kind === 'grok' ? '每週' : '7 天'}</span>
              <span className={`quota-row ${levelOf(seven)}`}>剩 {pctText(seven)}</span>
              <span className="quota-reset">{fmtTime(q?.seven_day?.resets_at)} 重置</span>
            </div>
          ) : null}
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

  /** Critical first, then warning, then by remaining; focused kind wins a tie; identities follow base. */
  const ordered = useMemo(() => {
    const rank: Record<Level, number> = { crit: 0, warn: 1, ok: 2 }
    return collectEntries(quota)
      .map((entry) => ({ entry, ...worst(quota[entry.key] ?? null) }))
      .sort((a, b) => {
        if (rank[a.level] !== rank[b.level]) return rank[a.level] - rank[b.level]
        const av = a.pct ?? 101
        const bv = b.pct ?? 101
        if (av !== bv) return av - bv
        if (a.entry.kind === focusKind && b.entry.kind !== focusKind) return -1
        if (b.entry.kind === focusKind && a.entry.kind !== focusKind) return 1
        if (a.entry.kind !== b.entry.kind) return kindRank(a.entry.kind) - kindRank(b.entry.kind)
        // Same kind: base row first, then identity name.
        if (!a.entry.identity && b.entry.identity) return -1
        if (a.entry.identity && !b.entry.identity) return 1
        return (a.entry.identity ?? '').localeCompare(b.entry.identity ?? '')
      })
      .map((x) => x.entry)
  }, [quota, focusKind])

  /** Popover lists every base kind, then any identity rows that have reported. */
  const popEntries = useMemo(() => {
    const base: QuotaEntry[] = BOT_KINDS.map((kind) => ({ key: kind, kind, identity: null }))
    const extras = collectEntries(quota).filter((e) => e.identity)
    extras.sort((a, b) => kindRank(a.kind) - kindRank(b.kind) || (a.identity ?? '').localeCompare(b.identity ?? ''))
    return [...base, ...extras]
  }, [quota])

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
        {ordered.map((entry) => (
          <Gauge key={entry.key} entry={entry} collapsed={collapsed} />
        ))}
      </button>
      {open ? (
        <div className="quota-pop" role="dialog" aria-label="所有額度">
          {popEntries.map((entry) => (
            <PopRow key={entry.key} entry={entry} />
          ))}
        </div>
      ) : null}
    </div>
  )
}
