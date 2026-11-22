import { useDismiss } from '../lib/useDismiss'
import { AGY_DEFAULT_IDENTITY, quotaIdentityLabel } from '../lib/defaultIdentity'
import { fmtDateTime } from '../lib/userTime'
import { CodexResetHint } from './CodexResetHint'
import { leftPct } from '../lib/quotaLeft'
import { QUOTA_FIT_START, nextQuotaFit, withFreshNeed } from '../lib/quotaLayout'
import type { QuotaFitState } from '../lib/quotaLayout'
import { measureQuotaAvail, measureQuotaRow } from '../lib/quotaMeasure'
import { Fragment, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, Identity, KindQuota, QuotaLimitHit, QuotaMap, QuotaResetCredits, QuotaWindow } from '../api/types'
import { LOCAL_HOST, quotaKey } from '../api/types'
import { identityPrefKey, kindPrefKey } from '../api'
import { identitiesOfHost, identityStatusOfHost, quotaClaimantsOf, toolsOfHost, useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import './mobileQuota.css'
import { isQuotaDisabled, quotaDisableKey, setQuotaDisabled, useDisabledQuota } from '../store/quotaHide'
import { quotaBaseKey, weeklyOnlyKind } from '../store/quotaLookup'
import { ApiBalanceGroup } from './ApiBalanceGroup'
import { BalanceQuotaChip } from './BalanceQuotaChip'
import { useClaudeBalanceVisible, useOpenAiBalanceVisible } from './balanceHooks'
import { OpenAiBalanceChip } from './OpenAiBalanceChip'
import { DEFAULT_ACCOUNT } from '../store/identityRows'
import { disableUntil } from '../lib/quotaDisableUntil'
import { KindIcon } from './KindTag'
import { KIND_LABEL } from './kindMeta'
import { QuotaLoginShell } from './QuotaLoginShell'
import { IdentityCliLogin, QuotaLoginSlash } from './QuotaLoginSlash'
import { UpdateQuotaChip } from './UpdateQuotaChip'
import { agyQuotaErrorText } from '../lib/agyQuotaError'
import { claudeExpiryState, formatExpiryTime } from '../lib/claudeLoginExpiry'
import { useAgyMissing } from './agyInstallState'
import { creditRows, creditUrgent, fmtCreditDate, fmtCreditIn, nearestCreditLeft } from '../lib/resetCredits'
import { RULE_5H, RULE_WEEKLY, fmtLeft, fmtLeftLong, msUntil, resetBadge } from '../lib/quotaReset'
import { CODEX_LOGIN_GUIDE, cliLoginCommand, identityEnv, shouldUseIdentityLogin } from '../lib/quotaLogin'
import './quotaLimitHit.css'
import { carriedOverAt } from '../lib/quotaWindowAge'
import { noFiveHourCap, projectNoFiveHour } from '../lib/quotaNoFiveHour'
import { limitHitWhen } from '../lib/limitHitWhen'
import './quotaStrip.css'
import { useUserTimeZone } from '../lib/userTime'

const EMPTY_EXPIRY: Record<string, number> = {}
const QUERYABLE: BotKind[] = ['claude', 'codex', 'grok', 'agy']

type QuotaEntry = { key: string; fullKey: string; kind: BotKind; identity: string | null }

function scopeToHost(quota: QuotaMap, host: string): QuotaMap {
  const out: QuotaMap = {}
  const prefix = `${host}/`
  for (const [k, v] of Object.entries(quota)) {
    if (host === LOCAL_HOST) {
      if (!k.includes('/')) out[k] = v
    } else if (k.startsWith(prefix)) {
      out[k.slice(prefix.length)] = v
    }
  }
  return out
}

function hostLabel(host: string, t: ReturnType<typeof useT>): string {
  return host === LOCAL_HOST ? t('本機') : host
}

function quotaTitle(host: string, t: ReturnType<typeof useT>): string {
  return host === LOCAL_HOST ? t('本機額度') : t('{host} 的額度', { host })
}

function useEntryQuota(entry: QuotaEntry): KindQuota | null {
  const raw = useStore((s) => s.quota[entry.fullKey] ?? null)
  return useMemo(() => projectNoFiveHour(entry.kind, raw), [entry.kind, raw])
}

function useLoggedOut(entry: QuotaEntry, host: string): boolean {
  return useStore((s) => {
    const name = entry.identity
    if (!name) return toolsOfHost(s, host)[entry.kind]?.logged_in === false
    return identityStatusOfHost(s, host)[name]?.logged_in === false
  })
}

type Level = 'crit' | 'warn' | 'ok'

function remaining(w: QuotaWindow | null | undefined): number | null {
  if (!w) return null
  return leftPct(w.used_pct)
}

function fmtPct(pct: number, hit = false): string {
  return pct === 0 && !hit ? '<1' : String(pct)
}

function levelOf(w: { low: boolean; critical: boolean } | null | undefined): Level {
  if (!w) return 'ok'
  if (w.critical) return 'crit'
  if (w.low) return 'warn'
  return 'ok'
}

function worst(q: KindQuota | null): Level {
  const windows = [q?.five_hour, q?.seven_day, q?.fable].filter((w): w is QuotaWindow => w != null)
  if (windows.some((w) => w.critical)) return 'crit'
  if (windows.some((w) => w.low)) return 'warn'
  return 'ok'
}

function fmtTime(iso: string | null | undefined): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '—'
  return fmtDateTime(d, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
}

function staleAge(iso: string, now: number, t: ReturnType<typeof useT>): string {
  const at = Date.parse(iso)
  if (Number.isNaN(at)) return ''
  const minutes = Math.max(0, Math.floor((now - at) / 60_000))
  if (minutes < 1) return t('不到 1 分鐘')
  if (minutes < 60) return t('{n} 分鐘', { n: minutes })
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return minutes % 60 ? t('{hours} 小時{minutes} 分鐘', { hours, minutes: minutes % 60 }) : t('{hours} 小時', { hours })
  const days = Math.floor(hours / 24)
  return hours % 24 ? t('{days} 天{hours} 小時', { days, hours: hours % 24 }) : t('{days} 天', { days })
}

function staleSuffix(q: KindQuota | null, now: number, t: ReturnType<typeof useT>): string {
  if (!q?.stale) return ''
  const age = staleAge(q.updated_at, now, t)
  return age ? t('（上次讀數，{age}前）', { age }) : t('（上次讀數）')
}

function entryLabel(entry: QuotaEntry): string {
  const identity = entry.identity ?? (entry.kind === 'agy' ? AGY_DEFAULT_IDENTITY : null)
  return identity ? `${KIND_LABEL[entry.kind]} · ${identity}` : KIND_LABEL[entry.kind]
}

function label(entry: QuotaEntry, q: KindQuota | null, loggedOut: boolean, now: number, t: ReturnType<typeof useT>): string {
  const parts = [entryLabel(entry)]
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const fable = remaining(q?.fable)
  const argon = entry.kind === 'agy' ? remaining(q?.argon_weekly) : null
  if (five === null && seven === null && fable === null && argon === null) parts.push(loggedOut ? t('這台主機偵測不到登入，額度尚未取得') : t('額度尚未取得'))
  const hit = Boolean(q?.limit_hit)
  if (five !== null) parts.push(t('5 小時剩餘 {pct}%', { pct: fmtPct(five, hit) }))
  if (seven !== null) {
    parts.push(weeklyOnlyKind(entry.kind) ? t('每週剩餘 {pct}%', { pct: fmtPct(seven, hit) }) : t('7 天剩餘 {pct}%', { pct: fmtPct(seven, hit) }))
  }
  if (fable !== null) parts.push(t('Fable 每週剩餘 {pct}%', { pct: fmtPct(fable, hit) }))
  if (argon !== null) {
    parts.push(t('Argon 每週剩餘 {pct}%', { pct: fmtPct(argon, hit) }))
    parts.push(t('Gemini 4 Argon 獨立週額度，不扣 Gemini 額度，沒有 5 小時限制'))
  }
  if (q?.five_hour?.resets_at) parts.push(t('5 小時 {time} 重置', { time: fmtTime(q.five_hour.resets_at) }))
  if (noFiveHourCap(entry.kind, q)) parts.push(t('5 小時無上限'))
  if (q?.seven_day?.resets_at) {
    parts.push(
      weeklyOnlyKind(entry.kind)
        ? t('每週 {time} 重置', { time: fmtTime(q.seven_day.resets_at) })
        : t('7 天 {time} 重置', { time: fmtTime(q.seven_day.resets_at) }),
    )
  }
  if (q?.fable?.resets_at) parts.push(t('Fable {time} 重置', { time: fmtTime(q.fable.resets_at) }))
  if (q?.argon_weekly?.resets_at) parts.push(t('Argon {time} 重置', { time: fmtTime(q.argon_weekly.resets_at) }))
  for (const [name, w] of [[t('5 小時'), q?.five_hour], [weekLabel(entry.kind) === '週' ? t('每週') : t('7 天'), q?.seven_day], ['Fable', q?.fable], ['Argon', q?.argon_weekly]] as const) {
    const at = carriedOverAt(w, q?.updated_at)
    if (!at) continue
    const age = staleAge(at, now, t)
    parts.push(age ? t('{name}是沿用 {age}前的讀數', { name, age }) : t('{name}是沿用的讀數', { name }))
  }
  if (q?.stale) parts.push(staleSuffix(q, now, t))
  return parts.join(t('，'))
}

function parseQuotaKey(key: string): QuotaEntry | null {
  const i = key.indexOf(':')
  const kind = (i === -1 ? key : key.slice(0, i)) as BotKind
  if (!QUERYABLE.includes(kind)) return null
  const identity = i === -1 ? null : key.slice(i + 1) || null
  if (identity !== null && !identity) return null
  return { key, fullKey: key, kind, identity }
}

function claudeIdentities(identities: Identity[]): Identity[] {
  return identities
    .filter((i) => i.kind === 'claude')
    .slice()
    .sort((a, b) => {
      if (a.name === 'cc0') return -1
      if (b.name === 'cc0') return 1
      return a.name.localeCompare(b.name, undefined, { numeric: true })
    })
}

function entryReactKey(entry: Omit<QuotaEntry, 'fullKey'>): string {
  return entry.identity ? `${entry.kind}:${entry.identity}` : entry.key
}

function collectEntries(quotaAll: QuotaMap, identities: Identity[], host: string): QuotaEntry[] {
  const quota = scopeToHost(quotaAll, host)
  const out: QuotaEntry[] = []
  const seenSlots = new Set<string>()
  const claudeIds = claudeIdentities(identities)

  const push = (entry: Omit<QuotaEntry, 'fullKey'>) => {
    const slot = entryReactKey(entry)
    if (seenSlots.has(slot)) return
    seenSlots.add(slot)
    out.push({ ...entry, fullKey: quotaKey(host, entry.key) })
  }

  if (claudeIds.length > 0) {
    for (const idn of claudeIds) push({ key: quotaBaseKey(quotaAll, host, 'claude', idn.name, claudeIds), kind: 'claude', identity: idn.name })
  } else if ('claude' in quota) {
    push({ key: 'claude', kind: 'claude', identity: null })
  }

  for (const key of Object.keys(quota).sort()) {
    const entry = parseQuotaKey(key)
    if (!entry || entry.kind !== 'claude' || !entry.identity) continue
    if (quota[key] == null) continue
    push(entry)
  }

  for (const kind of ['codex', 'grok', 'agy'] as const) {
    if (kind in quota) push({ key: kind, kind, identity: null })
  }

  const agyIds = identities.filter((i) => i.kind === 'agy')
  for (const idn of agyIds) push({ key: quotaBaseKey(quotaAll, host, 'agy', idn.name, agyIds), kind: 'agy', identity: idn.name })

  for (const key of Object.keys(quota).sort()) {
    const entry = parseQuotaKey(key)
    if (!entry || !entry.identity || entry.kind === 'claude' || entry.kind === 'agy') continue
    if (quota[key] == null) continue
    push(entry)
  }

  return out
}

function RiskDot({ level }: { level: Level }) {
  return <span className={`quota-risk ${level}`} aria-hidden="true" />
}

type WindowName = '5h' | '7d' | '週' | 'F' | 'Ar'

const WINDOW_MS: Record<WindowName, number> = {
  '5h': 5 * 3_600_000,
  '7d': 7 * 86_400_000,
  '週': 7 * 86_400_000,
  F: 7 * 86_400_000,
  Ar: 7 * 86_400_000,
}

function resetMark(resetsAt: string | null | undefined, span: number, now: number): number | null {
  if (!resetsAt) return null
  const t = new Date(resetsAt).getTime()
  if (Number.isNaN(t)) return null
  const left = t - now
  if (left <= 0) return 0
  return Math.min(100, (left / span) * 100)
}

function resetRule(name: WindowName) {
  return name === '5h' ? RULE_5H : RULE_WEEKLY
}

function soonLabel(name: WindowName, resetsAt: string | null, now: number): string | null {
  if (name !== '5h' || !resetsAt) return null
  const left = new Date(resetsAt).getTime() - now
  if (Number.isNaN(left) || left >= 60 * 60_000) return null
  return left <= 0 ? '0m' : `${Math.max(1, Math.ceil(left / 60_000))}m`
}

function useMinuteNow(): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000)
    return () => clearInterval(id)
  }, [])
  return now
}

function StaleWindowMark({ at, name, now }: { at: string; name: WindowName; now: number }) {
  const t = useT()
  const age = staleAge(at, now, t)
  return (
    <sup
      className="quota-window-stale"
      title={
        age
          ? t('{name} 這一格是沿用 {age}前的讀數——這幾次的狀態列沒有帶它，數字不一定是現況', { name, age })
          : t('{name} 這一格是沿用的讀數——這幾次的狀態列沒有帶它，數字不一定是現況', { name })
      }
    >
      {t('舊')}
    </sup>
  )
}

function Bar({
  pct,
  low,
  critical,
  mark,
  markTitle,
  unit,
  hit = false,
}: {
  pct: number | null
  low: boolean
  critical: boolean
  mark: number | null
  markTitle?: string
  hit?: boolean
  unit?: boolean
}) {
  const t = useT()
  const lv = levelOf({ low, critical })
  const zoom = pct !== null && pct < 10 && (mark === null || mark < 10)
  const bar = zoom ? (
    <span className="quota-bar-wrap zoomed" title={t('剩 {pct}%（條的刻度是 0–10%）', { pct: fmtPct(pct, hit) })}>
      <span className={`quota-bar ${lv}`}>
        <span className="quota-bar-fill" style={{ width: `${Math.min(100, pct * 10)}%` }} />
      </span>
    </span>
  ) : (
    <span className="quota-bar-wrap">
      <span className={`quota-bar ${lv}${pct === null ? ' nodata' : ''}`}>
        <span className="quota-bar-fill" style={{ width: pct === null ? '0%' : `${pct}%` }} />
      </span>
      {mark !== null ? <span className="quota-bar-mark" style={{ left: `${mark}%` }} title={markTitle} /> : null}
    </span>
  )
  return (
    <span className="quota-bar-row">
      {bar}
      {}
      <span className={`quota-bar-pct ${lv}${pct === null ? ' nodata' : ''}${unit || zoom ? ' has-unit' : ''}`} aria-hidden="true">
        {pct === null ? '—' : unit || zoom ? `${fmtPct(pct, hit)}%` : fmtPct(pct, hit)}
      </span>
    </span>
  )
}

const SHARED_RESET_MS = 60_000
function resetLineOf(rows: { name: WindowName; back: string; resetsAt: string }[], t: ReturnType<typeof useT>): { text: string; title: string } | null {
  if (rows.length === 0) return null
  const groups: { name: WindowName; back: string; at: number }[] = []
  for (const r of rows) {
    const at = new Date(r.resetsAt).getTime()
    if (!groups.some((g) => Math.abs(g.at - at) <= SHARED_RESET_MS)) groups.push({ name: r.name, back: r.back, at })
  }
  const title = rows.map((r) => t('{name} 還有 {back} 重置', { name: r.name, back: r.back })).join(t('；'))
  if (groups.length === 1) return { text: `└\u00a0${groups[0]!.back}`, title }
  return { text: `└${groups.map((g) => `${g.name}\u00a0${g.back}`).join('·')}`, title }
}

type WindowBar = { name: WindowName; pct: number | null; resetsAt: string | null; low: boolean; critical: boolean; staleAt: string | null }

function windowBar(name: WindowName, pct: number | null, w: QuotaWindow | null | undefined, updatedAt: string | undefined): WindowBar {
  return {
    name,
    pct,
    resetsAt: w?.resets_at ?? null,
    low: w?.low ?? false,
    critical: w?.critical ?? false,
    staleAt: carriedOverAt(w, updatedAt),
  }
}

function moreUrgent(w: WindowBar, best: WindowBar): boolean {
  const rank = (x: WindowBar) => (x.critical ? 2 : x.low ? 1 : 0)
  if (rank(w) !== rank(best)) return rank(w) > rank(best)
  return (w.pct ?? 100) < (best.pct ?? 100)
}

function windowText(name: WindowName, t: ReturnType<typeof useT>): string {
  if (name === '週') return t('週')
  if (name === 'Ar') return t('Ar')
  return name
}

function weekLabel(kind: BotKind): '7d' | '週' {
  return kind === 'grok' ? '週' : '7d'
}

function headPctOf(w: WindowBar | undefined, now: number, hit: boolean, t: ReturnType<typeof useT>) {
  if (!w) return null
  const back = resetBadge(w.pct, w.resetsAt, now, resetRule(w.name))
  if (!back) return null
  const text = w.pct === null ? t('無資料') : `${fmtPct(w.pct, hit)}%`
  return { win: w, text, title: w.pct !== null && (w.pct > 0 || !hit) ? t('剩 {pct}%，還有 {back} 重置', { pct: fmtPct(w.pct, hit), back }) : t('用完了，還有 {back} 重置', { back }) }
}

function Gauge({
  entry,
  host,
  compact,
  focused,
  open,
  onOpen,
}: {
  entry: QuotaEntry
  host: string
  compact: boolean
  focused: boolean
  open: boolean
  onOpen: () => void
}) {
  const t = useT()
  const q = useEntryQuota(entry)
  const loggedOut = useLoggedOut(entry, host)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const now = useMinuteNow()
  const borderWindows = compact ? [
    { edge: 'top', label: '5H', window: q?.five_hour, pct: five, mark: null },
    { edge: 'bottom', label: weekLabel(entry.kind).toUpperCase(), window: q?.seven_day, pct: seven, mark: resetMark(q?.seven_day?.resets_at, WINDOW_MS[weekLabel(entry.kind)], now) },
  ].filter((w) => w.pct !== null && Number.isFinite(w.pct)) : []
  const fable = remaining(q?.fable)
  const argon = entry.kind === 'agy' ? remaining(q?.argon_weekly) : null
  const noReading = five === null && seven === null && fable === null && argon === null
  const placeholder = loggedOut ? t('未登入') : t('查詢中…')
  const disabledMap = useDisabledQuota()
  const expiryMap = useStore((s) =>
    host === LOCAL_HOST ? s.localClaudeLoginExpiresAt : (s.hosts.find((h) => h.name === host)?.claude_login_expires_at ?? EMPTY_EXPIRY),
  )
  const claudeExpiry =
    entry.kind === 'claude' ? claudeExpiryState(expiryMap[entry.identity ?? 'cc0'], now) : null
  let windows: WindowBar[]
  if (compact && seven !== null) {
    const shown: WindowBar = windowBar(weekLabel(entry.kind), seven, q?.seven_day, q?.updated_at)
    const rivals: WindowBar[] = []
    if (five !== null && (q?.five_hour?.low || q?.five_hour?.critical)) {
      rivals.push(windowBar('5h', five, q?.five_hour, q?.updated_at))
    }
    if (fable !== null && (q?.fable?.low || q?.fable?.critical)) {
      rivals.push(windowBar('F', fable, q?.fable, q?.updated_at))
    }
    if (argon !== null && (q?.argon_weekly?.low || q?.argon_weekly?.critical)) {
      rivals.push(windowBar('Ar', argon, q?.argon_weekly, q?.updated_at))
    }
    windows = [rivals.reduce((best, w) => (moreUrgent(w, best) ? w : best), shown)]
  } else if (compact && seven === null && (five !== null || fable !== null || argon !== null)) {
    const cands: WindowBar[] = []
    if (five !== null) cands.push(windowBar('5h', five, q?.five_hour, q?.updated_at))
    if (fable !== null) cands.push(windowBar('F', fable, q?.fable, q?.updated_at))
    if (argon !== null) cands.push(windowBar('Ar', argon, q?.argon_weekly, q?.updated_at))
    windows = [cands.reduce((best, w) => (moreUrgent(w, best) ? w : best))]
  } else if (noReading) {
    windows = []
  } else {
    windows = []
    if (five !== null) {
      windows.push(windowBar('5h', five, q?.five_hour, q?.updated_at))
    }
    if (seven !== null) {
      windows.push(windowBar(weekLabel(entry.kind), seven, q?.seven_day, q?.updated_at))
    }
    if (fable !== null) {
      windows.push(windowBar('F', fable, q?.fable, q?.updated_at))
    }
    if (argon !== null) {
      windows.push(windowBar('Ar', argon, q?.argon_weekly, q?.updated_at))
    }
  }
  const hit = Boolean(q?.limit_hit)
  const winRow = (w: WindowBar, countdown: boolean) => {
    const span = WINDOW_MS[w.name]
    const mark = resetMark(w.resetsAt, span, now)
    const left = msUntil(w.resetsAt, now)
    const soon = countdown ? null : soonLabel(w.name, w.resetsAt, now)
    const rowTitle = w.name === 'Ar'
      ? left !== null
        ? `${t('Gemini 4 Argon 獨立週額度，不扣 Gemini 額度，沒有 5 小時限制')} (${t('{name} 還有 {back} 重置', { name: w.name, back: fmtLeft(left) })})`
        : t('Gemini 4 Argon 獨立週額度，不扣 Gemini 額度，沒有 5 小時限制')
      : soon
        ? t('{name} 還有 {soon} 重置', { name: w.name, soon })
        : undefined
    return (
      <span key={w.name} className="quota-window" title={w.name === 'Ar' ? rowTitle : undefined}>
        <span className={`quota-window-name${soon ? ' soon' : ''}${w.staleAt ? ' carried' : ''}`} title={rowTitle}>
          {soon ?? windowText(w.name, t)}
          {w.staleAt ? <StaleWindowMark at={w.staleAt} name={w.name} now={now} /> : null}
        </span>
        <Bar
          pct={w.pct}
          low={w.low}
          critical={w.critical}
          mark={mark}
          markTitle={left === null ? undefined : t('{name} 還有 {back} 重置', { name: w.name, back: fmtLeft(left) })}
          unit={countdown}
          hit={hit}
        />
      </span>
    )
  }
  const countdownRows = windows.flatMap((w) => {
    const back = resetBadge(w.pct, w.resetsAt, now, resetRule(w.name))
    return back && w.resetsAt ? [{ name: w.name, back, resetsAt: w.resetsAt, win: w }] : []
  })
  const headPct = compact ? headPctOf(windows[0], now, hit, t) : null
  const resetLine = compact ? null : resetLineOf(countdownRows, t)
  const resetLineLevel = countdownRows.some((r) => r.win.critical) ? 'crit' : countdownRows.some((r) => r.win.low) ? 'warn' : ''
  const uncapped = !compact && noFiveHourCap(entry.kind, q)
  const title = `${hostLabel(host, t)} · ${label(entry, q, loggedOut, now, t)}`
  const level = worst(q)
  const off = isQuotaDisabled(disabledMap, quotaDisableKey(host, entry.kind, entry.identity))
  const withOff = off ? `${title}${t('（已暫時停用，底下的 Bot 收在側欄外）')}` : title
  const blocked = q?.limit_hit ?? null
  const accessibleTitle = focused
    ? `${t('目前選取的 {title}', { title: withOff })}${borderWindows.map((w) => t('；{edge}邊框：{label} 剩餘 {pct}%', { edge: w.edge === 'top' ? t('上') : t('下'), label: w.label, pct: fmtPct(w.pct!, hit) })).join('')}`
    : withOff

  return (
    <span
      className={`quota-hp ${entry.kind} ${level}${focused ? ' focused' : ''}${borderWindows.length ? ' quota-framed' : ''}${off ? ' off' : ''}${blocked ? ' quota-blocked' : ''}${q?.stale ? ' stale' : ''}`}
      title={blocked ? `${accessibleTitle}\n\n${blockedLine(blocked, t)}` : accessibleTitle}
      aria-current={focused ? 'true' : undefined}
      onClick={(e) => {
        if ((e.target as HTMLElement).closest('input, button, a')) return
        onOpen()
      }}
    >
      {borderWindows.map((w) => (
        <span
          key={w.edge}
          className={`quota-border-meter ${w.edge}${w.window?.critical ? ' critical' : w.window?.low ? ' warn' : ''}`}
          role="progressbar"
          aria-label={t('{label} 剩餘額度（{edge}邊框）', { label: w.label, edge: w.edge === 'top' ? t('上') : t('下') })}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.min(100, Math.max(0, w.pct!))}
        >
          <span className="quota-border-fill" style={{ width: `${Math.min(100, Math.max(0, w.pct!))}%` }} />
          {}
          {w.edge === 'bottom' && w.mark !== null ? <span className="quota-border-mark" style={{ left: `${w.mark}%` }} /> : null}
        </span>
      ))}
      <span className="quota-head">
        <span className="quota-kind">
          {entry.kind === 'codex' ? <CreditBadge credits={q?.reset_credits} now={now} /> : null}
          <span aria-hidden="true" className="quota-kind-icon">
            <KindIcon kind={entry.kind} />
          </span>
          {claudeExpiry ? (
            <span
              className={`quota-expiry-badge ${claudeExpiry.stage}`}
              title={
                claudeExpiry.stage === 'expired'
                  ? t('Claude 登入已過期')
                  : t('Claude 登入將於 {time} 到期', { time: formatExpiryTime(claudeExpiry.expiresAtMs) })
              }
              aria-label={claudeExpiry.stage === 'expired' ? t('登入已過期') : t('登入即將到期')}
            >
              !
            </span>
          ) : null}
        </span>
        {compact && !entry.identity ? null : (
          <span className={`quota-identity${loggedOut ? ' logged-out' : ''}`} aria-hidden="true">
            {quotaIdentityLabel(entry.kind, entry.identity)}
          </span>
        )}
        {headPct ? (
          <span className={`quota-compact-pct quota-head-pct ${levelOf(headPct.win)}`} title={headPct.title}>
            {headPct.text}
          </span>
        ) : null}
        {entry.kind === 'codex' ? <CodexResetHint now={now} compact={compact} /> : null}
        {}
        <StripDisableToggle entry={entry} host={host} />
      </span>
      <button
        type="button"
        className="quota-bars-open"
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-label={accessibleTitle}
        onClick={onOpen}
      >
      {compact ? (
        <span className="quota-compact">
          {noReading ? (
            <span className="quota-compact-win nodata">
              <span className="quota-compact-pct quota-placeholder">{placeholder}</span>
            </span>
          ) : null}
          {windows.map((w) => {
            const back = resetBadge(w.pct, w.resetsAt, now, resetRule(w.name))
            const soon = back ? null : soonLabel(w.name, w.resetsAt, now)
            return (
            <span key={w.name} className={`quota-compact-win ${levelOf(w)}`}>
              <span className={`quota-window-name${soon ? ' soon' : ''}${w.staleAt ? ' carried' : ''}`} title={soon ? t('{name} 還有 {soon} 重置', { name: w.name, soon }) : undefined}>
                {soon ?? windowText(w.name, t)}
                {w.staleAt ? <StaleWindowMark at={w.staleAt} name={w.name} now={now} /> : null}
              </span>
              {back ? (
                <span className="quota-reset-at" title={w.pct !== null && (w.pct > 0 || !hit) ? t('剩 {pct}%，還有 {back} 重置', { pct: fmtPct(w.pct, hit), back }) : t('用完了，還有 {back} 重置', { back })}>
                  {back}
                </span>
              ) : (
                <span className="quota-compact-pct">{w.pct === null ? t('無資料') : `${fmtPct(w.pct, hit)}%`}</span>
              )}
            </span>
            )
          })}
        </span>
      ) : (
      <span className={`quota-bars${windows.length === 1 && !uncapped ? ' single' : ''}`}>
        {uncapped ? (
          <span className="quota-window">
            <span className="quota-window-name">5h</span>
            <span className="quota-bar-row">
              <span className="quota-no-cap" title={t('這個方案沒有 5 小時上限，只看週額度')}>{t('無上限')}</span>
            </span>
          </span>
        ) : null}
        {noReading ? <span className="quota-placeholder">{placeholder}</span> : null}
        {windows.map((w) => winRow(w, countdownRows.some((r) => r.name === w.name)))}
        {resetLine ? (
          <span className={`quota-reset-line ${resetLineLevel}`} title={resetLine.title}>
            {resetLine.text}
          </span>
        ) : null}
      </span>
      )}
      </button>
    </span>
  )
}

function DisableToggle({ on, label: name, onToggle }: { on: boolean; label: string; onToggle: () => void }) {
  const t = useT()
  return (
    <span className="quota-disable">
      <input
        type="checkbox"
        aria-label={t('暫時停用 {name}（底下的 Bot 先從側欄收起來，額度 reset 後自動回來）', { name })}
        checked={on}
        onChange={onToggle}
      />
      <span className="quota-disable-note" aria-hidden="true">
        {on ? t('已停用') : t('停用')}
      </span>
    </span>
  )
}

function StripDisableToggle({ entry, host }: { entry: QuotaEntry; host: string }) {
  const t = useT()
  const q = useEntryQuota(entry)
  const disabledMap = useDisabledQuota()
  const key = quotaDisableKey(host, entry.kind, entry.identity)
  const off = isQuotaDisabled(disabledMap, key)
  const short = entryLabel(entry)
  const name = `${hostLabel(host, t)} · ${short}`
  return (
    <label
      className={`quota-cell-toggle icon-tip${off ? ' on' : ''}`}
      data-tip={off ? t('{short} 已停用 · 額度 reset 後自動回來', { short }) : t('停用 {short} · 底下的 Bot 先收起來', { short })}
    >
      <input
        type="checkbox"
        aria-label={
          off
            ? t('解除停用 {name}（底下的 Bot 回到側欄）', { name })
            : t('停用 {name}（底下的 Bot 先從側欄收起來，額度 reset 後自動回來）', { name })
        }
        checked={off}
        onChange={() => setQuotaDisabled(key, !off, off ? null : disableUntil(q, Date.now()))}
      />
    </label>
  )
}

function PopLogin({ entry, host }: { entry: QuotaEntry; host: string }) {
  const t = useT()
  return entry.kind === 'codex' ? (
    <CodexShellLogin host={host} identity={entry.identity} />
  ) : (
    <QuotaLoginSlash kind={entry.kind} host={host} hostLabel={hostLabel(host, t)} identity={entry.identity} />
  )
}

function CodexShellLogin({ host, identity }: { host: string; identity: string | null }) {
  const t = useT()
  const command = useStore((s) => cliLoginCommand('codex', identityEnv(s, host, 'codex', identity)))
  if (shouldUseIdentityLogin(identity)) {
    return <IdentityCliLogin kind="codex" host={host} hostLabel={hostLabel(host, t)} identity={identity} />
  }
  return <QuotaLoginShell host={host} hostLabel={hostLabel(host, t)} kind="codex" command={command} guide={t.tr(CODEX_LOGIN_GUIDE)} />
}

function blockedLine(hit: QuotaLimitHit, t: ReturnType<typeof useT>): string {
  const when = limitHitWhen(hit.until)
  const back = when
    ? t('{when} 才會恢復', { when })
    : t('恢復時間 CLI 沒寫，要等它下一回合跑得動')
  return t('⛔ CLI 說這個帳號現在被擋住：{back}\n{message}', { back, message: hit.message })
}

function PopLimitHit({ hit }: { hit: QuotaLimitHit | null | undefined }) {
  const t = useT()
  useUserTimeZone()
  if (!hit) return null
  const when = limitHitWhen(hit.until)
  return (
    <div className="quota-pop-line limit-hit">
      <span className="quota-win">
        <span className="quota-ico" aria-hidden="true">
          ⛔
        </span>
        {t('被擋')}
      </span>
      <span className="quota-limit-hit" title={hit.message}>
        {when ? t('{when} 恢復', { when }) : t('等下一回合跑得動')}
        <span className="quota-limit-hit-why">{t('CLI 回報額度上限，量表是速率視窗，看不到這件事')}</span>
      </span>
    </div>
  )
}

function CreditBadge({ credits, now }: { credits: QuotaResetCredits | null | undefined; now: number }) {
  const t = useT()
  if (!credits || credits.available <= 0) return null
  const left = nearestCreditLeft(credits, now)
  const title =
    left === null
      ? t('{n} 張重置券可用', { n: credits.available })
      : t('{n} 張重置券可用，最近一張 {in}到期（{time}）', { n: credits.available, in: fmtCreditIn(left), time: fmtCreditDate(new Date(now + left).toISOString()) })
  return (
    <span className={`quota-credit-badge${creditUrgent(left) ? ' urgent' : ''}`} title={title} aria-label={title}>
      <svg className="quota-credit-icon" viewBox="0 0 12 12" aria-hidden="true" focusable="false">
        <path d="M1 3h10v2a1 1 0 0 0 0 2v2H1V7a1 1 0 0 0 0-2z" fill="none" stroke="currentColor" strokeWidth="1.1" strokeLinejoin="round" />
        <path d="M7.5 3.4v5.2" stroke="currentColor" strokeWidth="0.9" strokeDasharray="1 1" />
      </svg>
      {credits.available}
    </span>
  )
}

function PopResetCredits({ credits, now }: { credits: QuotaResetCredits | null | undefined; now: number }) {
  const t = useT()
  if (!credits || credits.available <= 0) return null
  const left = msUntil(credits.expires_at, now)
  const titleText = credits.title ?? t('額度重置券')
  const tip = left === null
    ? t('{title}・在 codex 裡用 /status → Reset usage', { title: titleText })
    : t('{title}・{left}後過期・在 codex 裡用 /status → Reset usage', { title: titleText, left: fmtLeft(left) })
  const rows = creditRows(credits, now)
  return (
    <>
      <div className="quota-pop-line reset-credits">
        <span className="quota-win">
          <span className="quota-ico" aria-hidden="true">
            ⟳
          </span>
          {t('重置券')}
        </span>
        <span className="quota-reset-credit" title={tip}>
          {t.x('{n} 張可用', {
            n: <strong>{credits.available}</strong>,
          })}
          {rows.length === 0 && credits.title ? <span className="quota-reset-title">{credits.title}</span> : null}
        </span>
      </div>
      {}
      {rows.map((r) => (
        <div key={r.id || r.expiresAt || r.title} className={`quota-pop-line reset-credit-row${creditUrgent(r.left) ? ' urgent' : ''}`}>
          <span className="quota-reset-title">{r.title ?? titleText}</span>
          <span className="quota-credit-when" title={t('到期時刻 {time}', { time: fmtCreditDate(r.expiresAt) })}>
            {r.expiresAt ? fmtCreditDate(r.expiresAt) : '—'}
            {r.left !== null ? <span className="quota-credit-in">{fmtCreditIn(r.left)}</span> : null}
          </span>
        </div>
      ))}
    </>
  )
}

function PopWindow({
  icon,
  name,
  win,
  w,
  now,
  hit,
}: {
  icon: string
  name: string
  win: WindowName
  w: QuotaWindow | null | undefined
  now: number
  hit: boolean
}) {
  const t = useT()
  const pct = remaining(w)
  if (pct === null) return null
  const left = msUntil(w?.resets_at, now)
  return (
    <div className="quota-pop-line">
      <span className="quota-win">
        <span className="quota-ico" aria-hidden="true">
          {icon}
        </span>
        {name}
      </span>
      <Bar
        pct={pct}
        low={w?.low ?? false}
        critical={w?.critical ?? false}
        mark={resetMark(w?.resets_at, WINDOW_MS[win], now)}
        markTitle={left === null ? undefined : t('{name} 還有 {back} 重置', { name, back: fmtLeft(left) })}
        hit={hit}
      />
      {}
      <span className="quota-reset" title={w?.resets_at ? t('重置時刻 {time}', { time: fmtTime(w.resets_at) }) : undefined}>
        <span className="quota-ico" aria-hidden="true">
          ↻
        </span>
        {left === null ? '—' : fmtLeftLong(left)}
      </span>
    </div>
  )
}

function PopRow({ entry, host }: { entry: QuotaEntry; host: string }) {
  const t = useT()
  const q = useEntryQuota(entry)
  const now = useMinuteNow()
  const known = useStore((s) => {
    if (entry.identity && quotaKey(host, `${entry.kind}:${entry.identity}`) in s.quota) return true
    return entry.fullKey in s.quota
  })
  const supported = QUERYABLE.includes(entry.kind)
  const loggedOut = useLoggedOut(entry, host)
  const agyMissing = useAgyMissing(host) && entry.kind === 'agy' && !entry.identity
  const agyQuotaError = useStore((s) => (entry.kind === 'agy' && !entry.identity ? toolsOfHost(s, host).agy?.quota_error ?? null : null))
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const fable = remaining(q?.fable)
  const disabledMap = useDisabledQuota()
  const key = quotaDisableKey(host, entry.kind, entry.identity)
  const off = isQuotaDisabled(disabledMap, key)
  const toggle = () => setQuotaDisabled(key, !off, off ? null : disableUntil(q, Date.now()))

  return (
    <div
      className={`quota-pop-row${off ? ' off' : ''}`}
      onClick={(e) => {
        if (!e.currentTarget.contains(e.target as Node)) return
        if ((e.target as HTMLElement).closest('input, button, a, select, textarea')) return
        toggle()
      }}
    >
      <div className="quota-pop-head">
        <span className={`quota-kind ${entry.kind}`} aria-hidden="true">
          <KindIcon kind={entry.kind} />
        </span>
        <span className="quota-name">{entryLabel(entry)}</span>
        {supported ? <RiskDot level={worst(q)} /> : null}
        {q?.plan ? <span className="quota-plan">{q.plan}</span> : null}
        <DisableToggle on={off} label={entryLabel(entry)} onToggle={toggle} />
      </div>
      {q?.stale ? <p className="quota-stale-note">{t('{note}，新的探測回來後會更新', { note: staleSuffix(q, now, t) })}</p> : null}
      {!supported ? (
        <p className="quota-pop-note">{t('CLI 不支援額度查詢')}</p>
      ) : agyMissing ? (
        <>
          <p className="quota-pop-note warn">{t('{host} 尚未安裝 agy，要自動安裝嗎？', { host: hostLabel(host, t) })}</p>
          <QuotaLoginSlash kind="agy" host={host} hostLabel={hostLabel(host, t)} identity={null} />
        </>
      ) : !known || (five === null && seven === null && fable === null && (entry.kind !== 'agy' || remaining(q?.argon_weekly) === null)) ? (
        <>
          <p className={`quota-pop-note${loggedOut || agyQuotaError ? ' warn' : ''}`}>
            {loggedOut
              ? t('{host} 上這個帳號未登入。', { host: hostLabel(host, t) })
              : agyQuotaError
                ? t('已登入，但額度暫時拿不到：{error}（{time} 起；會自動重試）', {
                    error: agyQuotaErrorText(agyQuotaError),
                    time: fmtTime(agyQuotaError.at),
                  })
                : entry.kind === 'agy' || weeklyOnlyKind(entry.kind)
                  ? t('背景查詢中')
                  : t('尚未取得（啟動一個 {kind} bot 後回報）', { kind: KIND_LABEL[entry.kind] })}
          </p>
          {loggedOut ? <PopLogin entry={entry} host={host} /> : null}
        </>
      ) : (
        entry.kind === 'agy' ? (
          <>
            <PopWindow icon="⏱" name="5h" win="5h" w={q?.five_hour} now={now} hit={Boolean(q?.limit_hit)} />
            <PopWindow icon="📅" name="7d" win="7d" w={q?.seven_day} now={now} hit={Boolean(q?.limit_hit)} />
            {q?.argon_weekly ? (
              <>
                <PopWindow icon="✦" name="Ar" win="Ar" w={q.argon_weekly} now={now} hit={Boolean(q?.limit_hit)} />
                <p className="quota-pop-note">{t('Gemini 4 Argon 獨立週額度，不扣 Gemini 額度，沒有 5 小時限制')}</p>
              </>
            ) : null}
            <PopLimitHit hit={q?.limit_hit} />
          </>
        ) : (
        <>
          {noFiveHourCap(entry.kind, q) ? (
            <div className="quota-pop-line">
              <span className="quota-win">
                <span className="quota-ico" aria-hidden="true">
                  ⏱
                </span>
                5h
              </span>
              <span className="quota-no-cap">{t('無上限')}</span>
              <span className="quota-reset">{t('這個方案沒有 5 小時上限，只看週額度')}</span>
            </div>
          ) : (
            <PopWindow icon="⏱" name="5h" win="5h" w={q?.five_hour} now={now} hit={Boolean(q?.limit_hit)} />
          )}
          <PopWindow icon="📅" name={weekLabel(entry.kind)} win="7d" w={q?.seven_day} now={now} hit={Boolean(q?.limit_hit)} />
          <PopWindow icon="✦" name="Fable" win="F" w={q?.fable} now={now} hit={Boolean(q?.limit_hit)} />
          <PopLimitHit hit={q?.limit_hit} />
          <PopResetCredits credits={q?.reset_credits} now={now} />
        </>
        )
      )}
      {}
      {loggedOut && supported && !agyMissing && known && (five !== null || seven !== null || fable !== null) ? (
        <>
          <p className="quota-pop-note warn">{t('{host} 上這個帳號未登入，上面是登出前的舊讀數。', { host: hostLabel(host, t) })}</p>
          <PopLogin entry={entry} host={host} />
        </>
      ) : null}
      {entry.kind === 'codex' ? <CodexResetHint now={now} detail /> : null}
      {entry.kind === 'claude' && !loggedOut ? <ClaudeExpiryPopNote host={host} identity={entry.identity ?? 'cc0'} now={now} /> : null}
    </div>
  )
}

function ClaudeExpiryPopNote({ host, identity, now }: { host: string; identity: string; now: number }) {
  const t = useT()
  const expiryMap = useStore((s) =>
    host === LOCAL_HOST ? s.localClaudeLoginExpiresAt : (s.hosts.find((h) => h.name === host)?.claude_login_expires_at ?? EMPTY_EXPIRY),
  )
  const loginIdentity = useStore((s) => s.loginIdentity)
  const st = claudeExpiryState(expiryMap[identity], now)
  if (!st) return null
  return (
    <>
      <p className={`quota-pop-note ${st.stage === 'expired' ? 'error' : 'warn'}`}>
        {st.stage === 'expired'
          ? t('{host} 上這個帳號的 Claude 登入已過期。', { host: hostLabel(host, t) })
          : t('{host} 上這個帳號的 Claude 登入將於 {time} 到期。', {
              host: hostLabel(host, t),
              time: formatExpiryTime(st.expiresAtMs),
            })}
      </p>
      <button type="button" className="quota-login-btn" onClick={() => void loginIdentity(host, identity)}>
        {t('重新登入')}
      </button>
    </>
  )
}
export function QuotaStrip({
  focusKind,
  focusIdentity,
  host = LOCAL_HOST,
}: {
  focusKind?: BotKind | null
  focusIdentity?: string | null
  host?: string
}) {
  const t = useT()
  const quota = useStore((s) => s.quota)
  const configured = useStore((s) => s.identities)
  const idStatus = useStore((s) => identityStatusOfHost(s, host))
  const identities = useMemo(() => identitiesOfHost(configured, idStatus, host ?? 'local'), [configured, idStatus, host])
  const [fitState, setFitState] = useState<{ key: string; s: QuotaFitState }>({ key: '', s: QUOTA_FIT_START })
  const [open, setOpen] = useState(false)
  const [only, setOnly] = useState<string | null>(null)
  const claudeBalanceVisible = useClaudeBalanceVisible()
  const openaiBalanceVisible = useOpenAiBalanceVisible()
  const phone = useMediaQuery(PHONE_QUERY)
  const [popTop, setPopTop] = useState<number | null>(null)
  const measure = () => {
    const r = wrap.current?.getBoundingClientRect()
    setPopTop(phone && r ? Math.round(r.bottom + 6) : null)
  }
  const wrap = useRef<HTMLDivElement>(null)
  const shellPane = useStore((s) => s.shellView?.paneId ?? null)
  const [seenShellPane, setSeenShellPane] = useState(shellPane)
  if (seenShellPane !== shellPane) {
    setSeenShellPane(shellPane)
    if (shellPane && open && phone) setOpen(false)
  }

  useDismiss(open, wrap, () => setOpen(false))

  useEffect(() => {
    if (!phone) return
    wrap.current?.querySelector('.quota-hp.focused')?.scrollIntoView({ inline: 'center', block: 'nearest' })
  }, [phone, focusKind, focusIdentity])

  const disabledIdentities = useStore((s) => s.disabledIdentities)
  const disabledKinds = useStore((s) => s.disabledKinds)
  const ordered = useMemo(() => {
    const live = quotaClaimantsOf(configured, idStatus, disabledIdentities, host)
    return collectEntries(quota, live, host).filter(
      (e) => !disabledIdentities.includes(identityPrefKey(host, e.kind, e.identity || DEFAULT_ACCOUNT)),
    ).filter((e) => !disabledKinds.includes(kindPrefKey(host, e.kind)))
  }, [quota, configured, idStatus, host, disabledIdentities, disabledKinds])

  const fitKey = `${host}|${ordered.map(entryReactKey).join(',')}`
  const fit = fitState.key === fitKey ? fitState.s : QUOTA_FIT_START

  const drawn = ordered.length > 0
  useLayoutEffect(() => {
    const strip = wrap.current
    const head = strip?.parentElement
    const row = strip?.querySelector<HTMLElement>('.quota-open')
    if (!drawn || phone || !strip || !head || !row) return
    const remeasure = () => {
      const groupEl = strip.querySelector<HTMLElement>('[data-k="quota-api-group"]')
      const groupGap = parseFloat(getComputedStyle(strip).columnGap) || 0
      const avail = Math.max(0, measureQuotaAvail(head, strip) - (groupEl ? groupEl.getBoundingClientRect().width + groupGap : 0))
      const wasCollapsed = row.classList.contains('collapsed')
      row.classList.remove('collapsed')
      let full
      try {
        full = measureQuotaRow(row)
      } finally {
        row.classList.toggle('collapsed', wasCollapsed)
      }
      const next = nextQuotaFit(withFreshNeed(fit, full.width), avail, full.width, false)
      row.classList.toggle('collapsed', next.level === 2)
      const current = next.level === 2 ? measureQuotaRow(row) : full
      row.classList.toggle('collapsed', wasCollapsed)
      row.style.setProperty('--quota-cell-min', `${current.cellMin}px`)
      if (next.level !== fit.level || next.need[0] !== fit.need[0] || next.need[1] !== fit.need[1]) {
        setFitState({ key: fitKey, s: next })
      }
    }
    remeasure()
    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(remeasure)
    for (const el of [head, ...head.children, ...row.children]) ro?.observe(el)
    const fonts = document.fonts
    fonts?.addEventListener('loadingdone', remeasure)
    return () => {
      ro?.disconnect()
      fonts?.removeEventListener('loadingdone', remeasure)
    }
  }, [drawn, phone, fit, fitKey, ordered])

  const popEntries = ordered

  if (ordered.length === 0 && !claudeBalanceVisible && !openaiBalanceVisible) return null

  const freshest = popEntries
    .map((e) => quota[e.fullKey]?.updated_at ?? null)
    .filter((x): x is string => Boolean(x))
    .sort()
    .pop()

  const collapsed = !phone && fit.level === 2
  const compact = phone
  const shown = ordered
  return (
    <div className="quota-strip" ref={wrap} aria-label={quotaTitle(host, t)}>
      {}
      <div className={`quota-open${collapsed ? ' collapsed' : ''}`}>
        {}
        {phone ? null : <UpdateQuotaChip />}
        {}
        {shown.map((entry) => {
          let focused = false
          if (focusKind && entry.kind === focusKind) {
            if (entry.kind !== 'claude') {
              const target = quotaBaseKey(quota, host, entry.kind, focusIdentity?.trim() || null, identities)
              focused = shown.some((e) => e.kind === entry.kind && e.key === target) ? entry.key === target : !entry.identity
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
          const onOpen = () => {
            const k = entryReactKey(entry)
            if (open && (!phone || only === k)) {
              setOpen(false)
              setOnly(null)
            } else {
              setOnly(phone ? k : null)
              measure()
              setOpen(true)
            }
          }
          return (
            <Fragment key={entryReactKey(entry)}>
              <Gauge
                entry={entry}
                host={host}
                compact={compact}
                focused={focused}
                open={open}
                onOpen={onOpen}
              />
            </Fragment>
          )
        })}
      </div>
      <ApiBalanceGroup />
      {open ? (
        <div className="quota-pop" role="dialog" aria-label={t('所有{title}', { title: quotaTitle(host, t) })} style={popTop !== null ? { top: popTop } : undefined}>
          <div className="quota-pop-title">{quotaTitle(host, t)}</div>
          {(only ? popEntries.filter((e) => entryReactKey(e) === only) : popEntries).map((entry) => (
            <PopRow key={entryReactKey(entry)} entry={entry} host={host} />
          ))}
          {only && popEntries.length > 1 ? (
            <button type="button" className="mini-btn quota-pop-all" onClick={() => setOnly(null)}>
              {t('看全部（{count}）', { count: popEntries.length })}
            </button>
          ) : null}
          {}
          {claudeBalanceVisible || openaiBalanceVisible ? (
            <div className="quota-pop-api" data-k="quota-pop-api">
              <div className="quota-pop-api-title">{t('API 餘額')}</div>
              <BalanceQuotaChip variant="row" />
              <OpenAiBalanceChip variant="row" />
            </div>
          ) : null}
          {freshest ? <p className="quota-pop-foot">{t('更新於 {time} · ↻ 是距離重置還有多久', { time: fmtTime(freshest) })}</p> : null}
        </div>
      ) : null}
    </div>
  )
}
