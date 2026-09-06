import { useEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, Identity, KindQuota, QuotaMap, QuotaWindow } from '../api/types'
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
 * Claude identities (`cc0`, `cc1`, …) each get their own gauge on the strip (icon + identity
 * label). The bare `claude` quota key is the default account — when a `cc0` (or empty-env)
 * identity exists it is shown as that label, not as an unlabeled Claude row.
 */

/** Every kind can report quota; grok arrives from the `/usage` probe. */
const QUERYABLE: BotKind[] = ['claude', 'codex', 'grok']

/** One strip / popover row: base kind or `kind:identity`. */
type QuotaEntry = { key: string; kind: BotKind; identity: string | null }

type Level = 'crit' | 'warn' | 'ok'

function remaining(w: QuotaWindow | null | undefined): number | null {
  return w ? Math.max(0, Math.round(100 - w.used_pct)) : null
}

/**
 * daemon 算好的旗標決定顏色（見 docs/API.md §12.4）——不在前端另外用 pct 寫死門檻，
 * 否則會跟 daemon 的 low/critical 各說各話（條紅了側欄卻沒警告，或反過來）。
 */
function levelOf(w: { low: boolean; critical: boolean } | null | undefined): Level {
  if (!w) return 'ok'
  if (w.critical) return 'crit'
  if (w.low) return 'warn'
  return 'ok'
}

/** 兩個窗口取最嚴重的旗標，一樣不碰 pct 數字。 */
function worst(q: KindQuota | null): Level {
  const windows = [q?.five_hour, q?.seven_day].filter((w): w is QuotaWindow => w != null)
  if (windows.some((w) => w.critical)) return 'crit'
  if (windows.some((w) => w.low)) return 'warn'
  return 'ok'
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
  if (seven !== null) {
    parts.push(entry.kind === 'grok' ? `每週剩餘 ${seven}%` : `7 天剩餘 ${seven}%`)
  }
  if (q?.five_hour?.resets_at) parts.push(`5 小時 ${fmtTime(q.five_hour.resets_at)} 重置`)
  if (q?.seven_day?.resets_at) {
    parts.push(
      entry.kind === 'grok'
        ? `每週 ${fmtTime(q.seven_day.resets_at)} 重置`
        : `7 天 ${fmtTime(q.seven_day.resets_at)} 重置`,
    )
  }
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

function claudeIdentities(identities: Identity[]): Identity[] {
  return identities
    .filter((i) => i.kind === 'claude')
    .slice()
    .sort((a, b) => {
      if (a.name === 'cc0') return -1
      if (b.name === 'cc0') return 1
      return a.name.localeCompare(b.name)
    })
}

/** Empty-env identity (typically `cc0`) shares the bare `claude` quota key with no-identity bots. */
function isDefaultClaudeIdentity(idn: Identity): boolean {
  return idn.name === 'cc0' || Object.keys(idn.env).length === 0
}

/**
 * Quota map key for a Claude identity. Prefers `claude:<name>`; the default/cc0 identity
 * falls back to bare `claude` when that is where statusline data landed.
 */
function claudeQuotaKey(quota: QuotaMap, idn: Identity, bareClaimed: boolean): { key: string; claimedBare: boolean } {
  const keyed = `claude:${idn.name}`
  if (quota[keyed] != null) return { key: keyed, claimedBare: false }
  if (!bareClaimed && isDefaultClaudeIdentity(idn) && 'claude' in quota) {
    return { key: 'claude', claimedBare: true }
  }
  return { key: keyed, claimedBare: false }
}

function entryReactKey(entry: QuotaEntry): string {
  return entry.identity ? `${entry.kind}:${entry.identity}` : entry.key
}

/**
 * Strip / popover entries in a **fixed** order — never by remaining %:
 *   cc0 → cc1 → (other claude identities) → codex → grok
 * When no Claude identities are configured, bare `claude` stands in for the Claude slot.
 */
function collectEntries(quota: QuotaMap, identities: Identity[]): QuotaEntry[] {
  const out: QuotaEntry[] = []
  const seenSlots = new Set<string>()
  const claudeIds = claudeIdentities(identities)

  const push = (entry: QuotaEntry) => {
    const slot = entryReactKey(entry)
    if (seenSlots.has(slot)) return
    seenSlots.add(slot)
    out.push(entry)
  }

  // 1) Claude identities first (cc0, cc1, …)
  if (claudeIds.length > 0) {
    let bareClaimed = false
    const bareOwner = claudeIds.find((i) => i.name === 'cc0') ?? claudeIds.find(isDefaultClaudeIdentity) ?? null
    for (const idn of claudeIds) {
      const useBare = bareOwner !== null && idn.name === bareOwner.name
      const resolved = useBare
        ? claudeQuotaKey(quota, idn, bareClaimed)
        : { key: `claude:${idn.name}`, claimedBare: false }
      if (resolved.claimedBare) bareClaimed = true
      push({ key: resolved.key, kind: 'claude', identity: idn.name })
    }
  } else if ('claude' in quota) {
    push({ key: 'claude', kind: 'claude', identity: null })
  }

  // Orphan claude:<id> keys not in the identities list (still after known ones, before codex)
  for (const key of Object.keys(quota).sort()) {
    const entry = parseQuotaKey(key)
    if (!entry || entry.kind !== 'claude' || !entry.identity) continue
    if (quota[key] == null) continue
    push(entry)
  }

  // 2) codex, then grok — fixed kind order, no remaining-% sort
  for (const kind of ['codex', 'grok'] as const) {
    if (kind in quota) push({ key: kind, kind, identity: null })
  }

  // Any other kind:identity orphans (future-proof), after the fixed kinds
  for (const key of Object.keys(quota).sort()) {
    const entry = parseQuotaKey(key)
    if (!entry || !entry.identity || entry.kind === 'claude') continue
    if (quota[key] == null) continue
    push(entry)
  }

  return out
}

/** Shape + colour, so the level survives greyscale and colour blindness. */
function RiskDot({ level }: { level: Level }) {
  return <span className={`quota-risk ${level}`} aria-hidden="true" />
}

/** 每個窗口有多長：位置刻度就是拿「離重置還有多久」去除這個。 */
const WINDOW_MS: Record<'5h' | '7d' | '週', number> = {
  '5h': 5 * 3_600_000,
  '7d': 7 * 86_400_000,
  '週': 7 * 86_400_000,
}

/**
 * 重置刻度在條上的位置：剩 3 小時、窗口 5 小時 → 60%。時間過去刻度就往左走，
 * 碰到左緣就是要重置了。拿不到 `resets_at` 時不畫。
 */
function resetMark(resetsAt: string | null | undefined, span: number, now: number): number | null {
  if (!resetsAt) return null
  const t = new Date(resetsAt).getTime()
  if (Number.isNaN(t)) return null
  const left = t - now
  if (left <= 0) return 0
  return Math.min(100, (left / span) * 100)
}

/** `2h13m` / `4d0h` / `12m`——和狀態列那條同一種寫法。 */
function fmtLeft(ms: number): string {
  if (ms <= 0) return '即將重置'
  const m = Math.floor(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return `${d}d${h % 24}h`
  if (h > 0) return `${h}h${m % 60}m`
  return `${m}m`
}

/** 每分鐘動一次就夠：刻度是分鐘級的。 */
function useMinuteNow(): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000)
    return () => clearInterval(id)
  }, [])
  return now
}

/** Health bar; daemon-decided `low` / `critical` (see docs/API.md §12.4) drive both colour and the countdown number. */
function Bar({
  pct,
  low,
  critical,
  mark,
  markTitle,
}: {
  pct: number | null
  low: boolean
  critical: boolean
  mark: number | null
  markTitle?: string
}) {
  const lv = levelOf({ low, critical })
  return (
    <span className="quota-bar-row">
      <span className="quota-bar-wrap">
        <span className={`quota-bar ${lv}${pct === null ? ' nodata' : ''}`}>
          <span className="quota-bar-fill" style={{ width: pct === null ? '0%' : `${pct}%` }} />
        </span>
        {/* 黑針＝下次 reset 的位置（剩餘時間 ÷ 窗口長度）。 */}
        {mark !== null ? <span className="quota-bar-mark" style={{ left: `${mark}%` }} title={markTitle} /> : null}
      </span>
      {low ? (
        <span className={`quota-bar-pct ${lv}`} aria-hidden="true">
          {pct}
        </span>
      ) : null}
    </span>
  )
}

type WindowBar = { name: '5h' | '7d' | '週'; pct: number | null; resetsAt: string | null; low: boolean; critical: boolean }

/** grok only reports a weekly window (stored in seven_day) — never call it 7d. */
function weekLabel(kind: BotKind): '7d' | '週' {
  return kind === 'grok' ? '週' : '7d'
}

/** Frameless, compact: icon above identity, bars to the right — keeps label glued to its bars. */
function Gauge({ entry, collapsed, focused }: { entry: QuotaEntry; collapsed: boolean; focused: boolean }) {
  const q = useStore((s) => {
    if (entry.identity) {
      const keyed = `${entry.kind}:${entry.identity}`
      if (s.quota[keyed] != null) return s.quota[keyed]
    }
    return s.quota[entry.key] ?? null
  })
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const now = useMinuteNow()
  // Named windows so 5h stays above 7d/週; collapsed shows only the worst.
  let windows: WindowBar[]
  if (collapsed) {
    const w = worstWindow(q)
    const src = w.name === '5h' ? q?.five_hour : q?.seven_day
    windows = [
      {
        name: w.name === '7d' ? weekLabel(entry.kind) : w.name,
        pct: w.pct,
        resetsAt: src?.resets_at ?? null,
        low: src?.low ?? false,
        critical: src?.critical ?? false,
      },
    ]
  } else if (five === null && seven === null) {
    windows = [{ name: entry.kind === 'grok' ? '週' : '5h', pct: null, resetsAt: null, low: false, critical: false }]
  } else {
    windows = []
    if (five !== null) {
      windows.push({
        name: '5h',
        pct: five,
        resetsAt: q?.five_hour?.resets_at ?? null,
        low: q?.five_hour?.low ?? false,
        critical: q?.five_hour?.critical ?? false,
      })
    }
    if (seven !== null) {
      windows.push({
        name: weekLabel(entry.kind),
        pct: seven,
        resetsAt: q?.seven_day?.resets_at ?? null,
        low: q?.seven_day?.low ?? false,
        critical: q?.seven_day?.critical ?? false,
      })
    }
  }
  const title = label(entry, q)
  const accessibleTitle = focused ? `目前選取的 ${title}` : title

  return (
    <span
      className={`quota-hp ${entry.kind} ${worst(q)}${focused ? ' focused' : ''}`}
      title={accessibleTitle}
      aria-label={accessibleTitle}
      aria-current={focused ? 'true' : undefined}
    >
      <span className="quota-head" aria-hidden="true">
        <span className="quota-kind">
          <KindIcon kind={entry.kind} />
        </span>
        {entry.identity ? <span className="quota-identity">{entry.identity}</span> : null}
      </span>
      <span className={`quota-bars${windows.length === 1 ? ' single' : ''}`}>
        {windows.map((w) => {
          const span = WINDOW_MS[w.name]
          const mark = resetMark(w.resetsAt, span, now)
          const left = w.resetsAt ? new Date(w.resetsAt).getTime() - now : null
          return (
            <span key={w.name} className="quota-window">
              <span className="quota-window-name">{w.name}</span>
              <Bar
                pct={w.pct}
                low={w.low}
                critical={w.critical}
                mark={mark}
                markTitle={left === null ? undefined : `${w.name} 還有 ${fmtLeft(left)} 重置`}
              />
            </span>
          )
        })}
      </span>
    </span>
  )
}

function PopRow({ entry }: { entry: QuotaEntry }) {
  const q = useStore((s) => {
    if (entry.identity) {
      const keyed = `${entry.kind}:${entry.identity}`
      if (s.quota[keyed] != null) return s.quota[keyed]
    }
    return s.quota[entry.key] ?? null
  })
  const known = useStore((s) => {
    if (entry.identity && `${entry.kind}:${entry.identity}` in s.quota) return true
    return entry.key in s.quota
  })
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
        {supported ? <RiskDot level={worst(q)} /> : null}
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
              <span className={`quota-row ${levelOf(q?.five_hour)}`}>剩 {pctText(five)}</span>
              <span className="quota-reset">{fmtTime(q?.five_hour?.resets_at)} 重置</span>
            </div>
          ) : null}
          {seven !== null ? (
            <div className="quota-pop-line">
              <span>{entry.kind === 'grok' ? '每週' : '7 天'}</span>
              <span className={`quota-row ${levelOf(q?.seven_day)}`}>剩 {pctText(seven)}</span>
              <span className="quota-reset">{fmtTime(q?.seven_day?.resets_at)} 重置</span>
            </div>
          ) : null}
          {q?.updated_at ? <p className="quota-pop-note">更新於 {fmtTime(q.updated_at)}</p> : null}
        </>
      )}
    </div>
  )
}

export function QuotaStrip({
  focusKind,
  focusIdentity,
}: {
  focusKind?: BotKind | null
  /** Selected bot's identity; null = default / cc0 account. */
  focusIdentity?: string | null
}) {
  const quota = useStore((s) => s.quota)
  const identities = useStore((s) => s.identities)
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

  /** Fixed: cc0 → cc1 → codex → grok. No remaining-% / focus reshuffle. */
  const ordered = useMemo(() => collectEntries(quota, identities), [quota, identities])

  /** Same fixed order as the strip (Claude identities expanded). */
  const popEntries = useMemo(() => collectEntries(quota, identities), [quota, identities])

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
        {ordered.map((entry) => {
          let focused = false
          if (focusKind && entry.kind === focusKind) {
            if (entry.kind !== 'claude') {
              focused = true
            } else {
              const focusId =
                focusIdentity && focusIdentity.trim()
                  ? focusIdentity.trim()
                  : claudeIdentities(identities).some((i) => i.name === 'cc0')
                    ? 'cc0'
                    : null
              focused = focusId ? entry.identity === focusId : !entry.identity
            }
          }
          return (
            <Gauge key={entryReactKey(entry)} entry={entry} collapsed={collapsed} focused={focused} />
          )
        })}
      </button>
      {open ? (
        <div className="quota-pop" role="dialog" aria-label="所有額度">
          {popEntries.map((entry) => (
            <PopRow key={entryReactKey(entry)} entry={entry} />
          ))}
        </div>
      ) : null}
    </div>
  )
}
