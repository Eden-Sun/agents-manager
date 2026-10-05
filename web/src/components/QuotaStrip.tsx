import { QUOTA_FIT_START, nextQuotaFit } from '../lib/quotaLayout'
import type { QuotaFitState } from '../lib/quotaLayout'
import { Fragment, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import type { BotKind, Identity, KindQuota, QuotaLimitHit, QuotaMap, QuotaResetCredits, QuotaWindow } from '../api/types'
import { LOCAL_HOST, quotaKey } from '../api/types'
import { identityPrefKey } from '../api'
import { identitiesOfHost, identityStatusOfHost, quotaClaimantsOf, toolsOfHost, useStore } from '../store/store'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import './mobileQuota.css'
import { isQuotaDisabled, quotaDisableKey, setQuotaDisabled, useDisabledQuota } from '../store/quotaHide'
import { AGY_CLAUDE_GPT, quotaBaseKey, weeklyOnlyKind } from '../store/quotaLookup'
import { KindIcon } from './KindTag'
import { KIND_LABEL } from './kindMeta'
import { QuotaLoginShell } from './QuotaLoginShell'
import { IdentityCliLogin, QuotaLoginSlash } from './QuotaLoginSlash'
import { UpdateQuotaChip } from './UpdateQuotaChip'
import { AgyLogoutButton, AgyLogoutDialog } from './AgyLogout'
import { RULE_5H, RULE_WEEKLY, resetBadge } from '../lib/quotaReset'
import { cliLoginCommand, identityEnv, shouldUseIdentityLogin } from '../lib/quotaLogin'
import './quotaLimitHit.css'
import { carriedOverAt } from '../lib/quotaWindowAge'
import './quotaStrip.css'

/**
 * Remaining quota per kind (`GET /api/quota` + WS `quota_updated`). Every kind stays visible;
 * only its windows collapse. Fill length carries the level so it survives greyscale.
 * 桌機每個帳號都畫完整量表，不收進 `+N`（2026-09-11 使用者：「額度顯示是很重要的訊息，不要去省他的空間」）。
 * grok 只有週窗；agy 的 Gemini 與 Claude+GPT 各有 5h／週窗口，每組顯示較緊的一窗，popover 有完整明細。
 * 兩組合成一格、維持 `G`、`C+G` 兩條（2026-10-04 使用者：「兩種放一起顯示，不然佔空間」）。
 * 額度按主機分開（SPEC §14），一次只顯示一台；遠端 key 帶 `<host>/` 前綴，先投影成裸 key。
 */

const QUERYABLE: BotKind[] = ['claude', 'codex', 'grok', 'agy']

type QuotaEntry = { key: string; fullKey: string; kind: BotKind; identity: string | null }

/** `m4p/claude:cc1` → `claude:cc1`，只留屬於 `host` 的。 */
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

function hostLabel(host: string): string {
  return host === LOCAL_HOST ? '本機' : host
}

function quotaTitle(host: string): string {
  return host === LOCAL_HOST ? '本機額度' : `${host} 的額度`
}

function useEntryQuota(entry: QuotaEntry, host: string): KindQuota | null {
  return useStore((s) => {
    if (entry.identity) {
      const keyed = quotaKey(host, `${entry.kind}:${entry.identity}`)
      if (s.quota[keyed] != null) return s.quota[keyed]
    }
    return s.quota[entry.fullKey] ?? null
  })
}

/** 看該 host 的 `identity_status`（同名身份在各主機可能是不同帳號）；Gauge 與 PopRow 共用以免判斷漂移。 */
function useLoggedOut(entry: QuotaEntry, host: string): boolean {
  return useStore((s) => {
    const name = entry.identity
    // 預設帳號看 `tools.<kind>.logged_in`，否則沒登入會永遠顯示「背景查詢中」。
    if (!name) return toolsOfHost(s, host)[entry.kind]?.logged_in === false
    return identityStatusOfHost(s, host)[name]?.logged_in === false
  })
}

type Level = 'crit' | 'warn' | 'ok'

/** 不到 10 留一位小數：快用完時 9.8 與 9.1 差一整回合（2026-09-08）。 */
function remaining(w: QuotaWindow | null | undefined): number | null {
  if (!w) return null
  const left = Math.max(0, 100 - w.used_pct)
  return left < 10 ? Math.round(left * 10) / 10 : Math.round(left)
}

function fmtPct(pct: number): string {
  return String(pct)
}

/** 顏色吃 daemon 旗標（docs/API.md §12.4），前端不另寫 pct 門檻以免跟側欄警告不一致。 */
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

/** The window closest to running out — what the collapsed pill shows. */
function worstWindow(q: KindQuota | null): { name: WindowName; pct: number | null } {
  const cands: { name: WindowName; pct: number }[] = []
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const fable = remaining(q?.fable)
  if (five !== null) cands.push({ name: '5h', pct: five })
  if (seven !== null) cands.push({ name: '7d', pct: seven })
  if (fable !== null) cands.push({ name: 'F', pct: fable })
  if (cands.length === 0) return { name: '5h', pct: null }
  return cands.reduce((a, b) => (b.pct < a.pct ? b : a))
}

function fmtTime(iso: string | null | undefined): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '—'
  return d.toLocaleString([], { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
}

function staleAge(iso: string, now: number): string {
  const at = Date.parse(iso)
  if (Number.isNaN(at)) return ''
  const minutes = Math.max(0, Math.floor((now - at) / 60_000))
  if (minutes < 1) return '不到 1 分鐘'
  if (minutes < 60) return `${minutes} 分鐘`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小時${minutes % 60 ? `${minutes % 60} 分鐘` : ''}`
  const days = Math.floor(hours / 24)
  return `${days} 天${hours % 24 ? `${hours % 24} 小時` : ''}`
}

function staleSuffix(q: KindQuota | null, now: number): string {
  if (!q?.stale) return ''
  const age = staleAge(q.updated_at, now)
  return `（上次讀數${age ? `，${age}前` : ''}）`
}

/** agy 的第二個桶（Claude+GPT）跟 Gemini 畫在同一格（`agy` 那格），讀它的 store key。 */
function useAgyClaudeGpt(entry: QuotaEntry, host: string): KindQuota | null {
  return useStore((s) => (entry.kind === 'agy' ? s.quota[quotaKey(host, `agy:${AGY_CLAUDE_GPT}`)] ?? null : null))
}

/** agy 一格兩條：Gemini＝`agy`、Claude+GPT＝`agy:claude-gpt`，各選 5h／週中較緊的一窗。 */
const AGY_BUCKETS = [
  { win: 'G', name: 'Gemini' },
  { win: 'C+G', name: 'Claude+GPT' },
] as const

function agyLabel(q: KindQuota | null, q2: KindQuota | null): string {
  const parts: string[] = [KIND_LABEL.agy]
  for (const [b, selected] of [[AGY_BUCKETS[0], agyWindow(q, 'G')], [AGY_BUCKETS[1], agyWindow(q2, 'C+G')]] as const) {
    if (!selected || selected.pct === null) continue
    const windowName = selected.name === '週' ? '每週' : selected.name
    parts.push(`${b.name} ${windowName}剩餘 ${selected.pct}%${selected.resetsAt ? `（${fmtTime(selected.resetsAt)} 重置）` : ''}`)
  }
  if (parts.length === 1) parts.push('額度尚未取得')
  return parts.join('，')
}

function worse(a: 'ok' | 'warn' | 'crit', b: 'ok' | 'warn' | 'crit'): 'ok' | 'warn' | 'crit' {
  const r = { ok: 0, warn: 1, crit: 2 }
  return r[b] > r[a] ? b : a
}

function entryLabel(entry: QuotaEntry): string {
  return entry.identity ? `${KIND_LABEL[entry.kind]} · ${entry.identity}` : KIND_LABEL[entry.kind]
}

function label(entry: QuotaEntry, q: KindQuota | null, loggedOut = false, now = Date.now()): string {
  const parts = [entryLabel(entry)]
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  if (five === null && seven === null) parts.push(loggedOut ? '這台主機偵測不到登入，額度尚未取得' : '額度尚未取得')
  if (five !== null) parts.push(`5 小時剩餘 ${five}%`)
  if (seven !== null) {
    parts.push(weeklyOnlyKind(entry.kind) ? `每週剩餘 ${seven}%` : `7 天剩餘 ${seven}%`)
  }
  const fable = remaining(q?.fable)
  if (fable !== null) parts.push(`Fable 每週剩餘 ${fable}%`)
  if (q?.five_hour?.resets_at) parts.push(`5 小時 ${fmtTime(q.five_hour.resets_at)} 重置`)
  if (q?.seven_day?.resets_at) {
    parts.push(
      weeklyOnlyKind(entry.kind)
        ? `每週 ${fmtTime(q.seven_day.resets_at)} 重置`
        : `7 天 ${fmtTime(q.seven_day.resets_at)} 重置`,
    )
  }
  if (q?.fable?.resets_at) parts.push(`Fable ${fmtTime(q.fable.resets_at)} 重置`)
  // 沿用上一份的那幾桶要點名（#540）：整筆的 `stale` 講的是另一件事（開機從快取回填）。
  for (const [name, w] of [['5 小時', q?.five_hour], [weekLabel(entry.kind) === '週' ? '每週' : '7 天', q?.seven_day], ['Fable', q?.fable]] as const) {
    const at = carriedOverAt(w, q?.updated_at)
    if (!at) continue
    const age = staleAge(at, now)
    parts.push(`${name}是沿用${age ? ` ${age}前` : ''}的讀數`)
  }
  if (q?.stale) parts.push(staleSuffix(q, now))
  return parts.join('，')
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
      return a.name.localeCompare(b.name)
    })
}

function entryReactKey(entry: Omit<QuotaEntry, 'fullKey'>): string {
  return entry.identity ? `${entry.kind}:${entry.identity}` : entry.key
}

/** Fixed order, never by remaining %: cc0 → cc1 → other claude → codex → grok. */
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
    // 落點（含裸 key 的互斥）只有 `store/quotaLookup` 一份規則，側欄的反灰與黃燈查的是同一支。
    for (const idn of claudeIds) push({ key: quotaBaseKey(quotaAll, host, 'claude', idn.name, claudeIds), kind: 'claude', identity: idn.name })
  } else if ('claude' in quota) {
    push({ key: 'claude', kind: 'claude', identity: null })
  }

  // Orphan claude:<id> keys not in the identities list
  for (const key of Object.keys(quota).sort()) {
    const entry = parseQuotaKey(key)
    if (!entry || entry.kind !== 'claude' || !entry.identity) continue
    if (quota[key] == null) continue
    push(entry)
  }

  for (const kind of ['codex', 'grok', 'agy'] as const) {
    if (kind in quota) push({ key: kind, kind, identity: null })
  }

  for (const key of Object.keys(quota).sort()) {
    const entry = parseQuotaKey(key)
    // agy 的 claude-gpt 桶畫在 `agy` 那格裡，不另開一格。
    if (!entry || !entry.identity || entry.kind === 'claude' || entry.kind === 'agy') continue
    if (quota[key] == null) continue
    push(entry)
  }

  return out
}

/** Shape + colour, so the level survives greyscale and colour blindness. */
function RiskDot({ level }: { level: Level }) {
  return <span className={`quota-risk ${level}`} aria-hidden="true" />
}

/** `F` 是 Max 方案的 Fable 週窗，只有 claude 有。 */
type WindowName = '5h' | '7d' | '週' | 'F'

const WINDOW_MS: Record<WindowName, number> = {
  '5h': 5 * 3_600_000,
  '7d': 7 * 86_400_000,
  '週': 7 * 86_400_000,
  F: 7 * 86_400_000,
}

/** 重置刻度位置＝剩餘時間 ÷ 窗口長度（剩 3h／5h → 60%）。 */
function resetMark(resetsAt: string | null | undefined, span: number, now: number): number | null {
  if (!resetsAt) return null
  const t = new Date(resetsAt).getTime()
  if (Number.isNaN(t)) return null
  const left = t - now
  if (left <= 0) return 0
  return Math.min(100, (left / span) * 100)
}

/** `2h13m` / `4d0h` / `12m`——和狀態列同一種寫法。 */
function fmtLeft(ms: number): string {
  if (ms <= 0) return '即將重置'
  const m = Math.floor(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return `${d}d${h % 24}h`
  if (h > 0) return `${h}h${m % 60}m`
  return `${m}m`
}

/** popover 用的長寫法（`2 天 3 小時`）；`fmtLeft` 是窄處的緊縮版。 */
function fmtLeftLong(ms: number): string {
  if (ms <= 0) return '即將重置'
  const m = Math.floor(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return `${d} 天 ${h % 24} 小時`
  if (h > 0) return `${h} 小時 ${m % 60} 分`
  return `${m} 分`
}

/** 什麼時候把百分比換成倒數：5h 用完且三小時內；週窗口（7d／週／Fable）剩不到 10% 且 24 小時內（2026-09-17 使用者）。 */
function resetRule(name: WindowName) {
  return name === '5h' ? RULE_5H : RULE_WEEKLY
}

/** 5h 不到一小時就重置時窗口名換成剩幾分（`12m`），讓使用者提早準備（2026-09-09）；只做 5h。 */
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

/**
 * 沿用上一份讀數的那一格（#540）：窗口名後面掛一個「舊」，提示寫出它自己的年齡。
 * 只用顏色不行（UI-DECISIONS：狀態一律還有文字、形狀或記號），所以是字不是點。
 */
function StaleWindowMark({ at, name, now }: { at: string; name: WindowName; now: number }) {
  const age = staleAge(at, now)
  return (
    <sup className="quota-window-stale" title={`${name} 這一格是沿用${age ? ` ${age}前` : ''}的讀數——這幾次的狀態列沒有帶它，數字不一定是現況`}>
      舊
    </sup>
  )
}

/** Health bar; colour follows daemon `low` / `critical` (docs/API.md §12.4). */
function Bar({
  pct,
  low,
  critical,
  mark,
  markTitle,
  instead,
}: {
  pct: number | null
  low: boolean
  critical: boolean
  mark: number | null
  markTitle?: string
  /** 快回來的那條：右邊的百分比換成倒數（2026-09-14 使用者）；這時**量表條也不畫**（2026-09-17 使用者：
   *  剩不到 10% 的條又短又細，看不出東西，只要數字），百分比照樣印。 */
  instead?: ReactNode
}) {
  const lv = levelOf({ low, critical })
  // 條拿掉、數字留著（同日使用者：「雖然除去條，但也要數字」）：倒數在前，百分比照舊留在最右那一欄。
  if (instead)
    return (
      <span className="quota-bar-row countdown">
        {instead}
        {/* 倒數那列旁邊是 `21h22` 這種數字，裸的 `0` 讀不出是百分比：這裡帶 `%`（2026-10-02 使用者）。 */}
        <span className={`quota-bar-pct ${lv}${pct === null ? ' nodata' : ''}`} aria-hidden="true">
          {pct === null ? '—' : `${fmtPct(pct)}%`}
        </span>
      </span>
    )
  return (
    <span className="quota-bar-row">
      <span className="quota-bar-wrap">
        <span className={`quota-bar ${lv}${pct === null ? ' nodata' : ''}`}>
          <span className="quota-bar-fill" style={{ width: pct === null ? '0%' : `${pct}%` }} />
        </span>
        {mark !== null ? <span className="quota-bar-mark" style={{ left: `${mark}%` }} title={markTitle} /> : null}
      </span>
      {/* 數字一律印（UI-DECISIONS：百分比始終保留；2026-09-09 使用者截圖）。歸零那條例外：
          0 不會再變，換成「幾點回來 · 還有多久」才是這時唯一還在動的數字。 */}
      {instead ?? (
        <span className={`quota-bar-pct ${lv}${pct === null ? ' nodata' : ''}`} aria-hidden="true">
          {pct === null ? '—' : fmtPct(pct)}
        </span>
      )}
    </span>
  )
}

/** `staleAt`：這一桶是沿用上一份讀數時，它自己的觀測時間（#540）；不是沿用就 `null`。 */
type AgyGroup = 'G' | 'C+G'
type WindowBar = { name: WindowName; pct: number | null; resetsAt: string | null; low: boolean; critical: boolean; staleAt: string | null; group?: AgyGroup; sourceWindow: QuotaWindow | null }

/** 六個建構點共用：`low`／`critical` 一律照 daemon 的旗標，沿用與否照 `observed_at`。 */
function windowBar(name: WindowName, pct: number | null, w: QuotaWindow | null | undefined, updatedAt: string | undefined, group?: AgyGroup): WindowBar {
  return {
    name,
    pct,
    resetsAt: w?.resets_at ?? null,
    low: w?.low ?? false,
    critical: w?.critical ?? false,
    staleAt: carriedOverAt(w, updatedAt),
    group,
    sourceWindow: w ?? null,
  }
}

/** 手機一格只放一個數字：先比 daemon 旗標再比 pct；平手留現任，免得數字在窗口間跳。 */
function moreUrgent(w: WindowBar, best: WindowBar): boolean {
  const rank = (x: WindowBar) => (x.critical ? 2 : x.low ? 1 : 0)
  if (rank(w) !== rank(best)) return rank(w) > rank(best)
  return (w.pct ?? 100) < (best.pct ?? 100)
}

function agyWindow(q: KindQuota | null, group: AgyGroup): WindowBar | null {
  if (!q) return null
  const candidates = [
    q.five_hour ? windowBar('5h', remaining(q.five_hour), q.five_hour, q.updated_at, group) : null,
    q.seven_day ? windowBar('週', remaining(q.seven_day), q.seven_day, q.updated_at, group) : null,
  ].filter((w): w is WindowBar => w !== null && w.pct !== null)
  return candidates.reduce<WindowBar | null>((best, w) => !best || moreUrgent(w, best) ? w : best, null)
}

function windowLabel(w: WindowBar, soon?: string | null): string {
  if (w.group) return `${w.group} ${w.name}${soon ? ` ${soon}` : ''}`
  return soon ?? w.name
}

/** grok reports only a weekly window; agy's weekly window is stored in seven_day and labelled 週. */
function weekLabel(kind: BotKind): '7d' | '週' {
  return weeklyOnlyKind(kind) ? '週' : '7d'
}

function Gauge({
  entry,
  host,
  collapsed,
  compact,
  focused,
  open,
  onOpen,
}: {
  entry: QuotaEntry
  host: string
  collapsed: boolean
  /** 手機：條子縮成 chip，剩餘量用數字寫。 */
  compact: boolean
  focused: boolean
  open: boolean
  onOpen: () => void
}) {
  const q = useEntryQuota(entry, host)
  const q2 = useAgyClaudeGpt(entry, host)
  const agy = entry.kind === 'agy'
  const loggedOut = useLoggedOut(entry, host)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const agyGemini = agyWindow(q, 'G')
  const agyClaudeGpt = agyWindow(q2, 'C+G')
  // 上下邊框量表只有手機畫（2026-09-11 使用者：桌機已有 bar），且每一格都畫（2026-09-13 使用者）。
  const borderWindows = compact ? (agy ? [
    agyGemini ? { edge: 'top', label: windowLabel(agyGemini), window: agyGemini.sourceWindow, pct: agyGemini.pct } : null,
    agyClaudeGpt ? { edge: 'bottom', label: windowLabel(agyClaudeGpt), window: agyClaudeGpt.sourceWindow, pct: agyClaudeGpt.pct } : null,
  ] : [
    { edge: 'top', label: '5H', window: q?.five_hour, pct: five },
    { edge: 'bottom', label: weekLabel(entry.kind).toUpperCase(), window: q?.seven_day, pct: seven },
  ]).filter((w): w is NonNullable<typeof w> => w !== null && w.pct !== null && Number.isFinite(w.pct)) : []
  const fable = remaining(q?.fable)
  const now = useMinuteNow()
  const disabledMap = useDisabledQuota()
  let windows: WindowBar[]
  if (agy) {
    // 每個模型組各保留一條：顯示低/critical 優先、否則剩餘比例較低的窗口。
    windows = [agyGemini ?? windowBar('週', null, q?.seven_day, q?.updated_at, 'G')]
    if (agyClaudeGpt) windows.push(agyClaudeGpt)
    else if (q2) windows.push(windowBar('週', null, q2.seven_day, q2.updated_at, 'C+G'))
  } else
  // 手機一格只寫一個窗口，否則 390px 放五格會長高（2026-09-12 使用者）。預設 7d（決定今天能否開工），
  // 5h／F 被 daemon 標 low／critical 且更急時才取代，不並列；完整數字在 tooltip 與 sheet。
  if (compact && seven !== null) {
    const shown: WindowBar = windowBar(weekLabel(entry.kind), seven, q?.seven_day, q?.updated_at)
    // 門檻見 docs/API.md §12.4。
    const rivals: WindowBar[] = []
    if (five !== null && (q?.five_hour?.low || q?.five_hour?.critical)) {
      rivals.push(windowBar('5h', five, q?.five_hour, q?.updated_at))
    }
    if (fable !== null && (q?.fable?.low || q?.fable?.critical)) {
      rivals.push(windowBar('F', fable, q?.fable, q?.updated_at))
    }
    windows = [rivals.reduce((best, w) => (moreUrgent(w, best) ? w : best), shown)]
  } else if (collapsed) {
    const w = worstWindow(q)
    const src = w.name === '5h' ? q?.five_hour : w.name === 'F' ? q?.fable : q?.seven_day
    windows = [windowBar(w.name === '7d' ? weekLabel(entry.kind) : w.name, w.pct, src, q?.updated_at)]
  } else if (five === null && seven === null) {
    windows = [windowBar(weeklyOnlyKind(entry.kind) ? '週' : '5h', null, null, q?.updated_at)]
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
  }
  const title = `${hostLabel(host)} · ${agy ? agyLabel(q, q2) : label(entry, q, loggedOut, now)}`
  const level = agy ? worse(worst(q), worst(q2)) : worst(q)
  const off = isQuotaDisabled(disabledMap, quotaDisableKey(host, entry.kind, entry.identity))
  const withOff = off ? `${title}（已暫時停用，底下的 Bot 收在側欄外）` : title
  // 量表是速率視窗，codex credits 用完時仍滿格卻一直 hit limit，所以畫在格子上（2026-09-12 使用者）。
  const blocked = q?.limit_hit ?? (agy ? q2?.limit_hit : null) ?? null
  const accessibleTitle = focused
    ? `目前選取的 ${withOff}${borderWindows.map((w) => `；${w.edge === 'top' ? '上' : '下'}邊框：${w.label} 剩餘 ${fmtPct(w.pct!)}%`).join('')}`
    : withOff

  return (
    <span
      className={`quota-hp ${entry.kind} ${level}${focused ? ' focused' : ''}${borderWindows.length ? ' quota-framed' : ''}${off ? ' off' : ''}${blocked ? ' quota-blocked' : ''}${q?.stale ? ' stale' : ''}`}
      title={blocked ? `${accessibleTitle}\n\n${blockedLine(blocked)}` : accessibleTitle}
      aria-current={focused ? 'true' : undefined}
      // input／button 的點擊放行，否則量表按鈕會一次開一次關。
      onClick={(e) => {
        if ((e.target as HTMLElement).closest('input, button')) return
        onOpen()
      }}
    >
      {borderWindows.map((w) => (
        <span
          key={w.edge}
          className={`quota-border-meter ${w.edge}${w.window?.critical ? ' critical' : w.window?.low ? ' warn' : ''}`}
          role="progressbar"
          aria-label={`${w.label} 剩餘額度（${w.edge === 'top' ? '上' : '下'}邊框）`}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={Math.min(100, Math.max(0, w.pct!))}
        >
          <span className="quota-border-fill" style={{ width: `${Math.min(100, Math.max(0, w.pct!))}%` }} />
        </span>
      ))}
      <span className="quota-head">
        <span className="quota-kind" aria-hidden="true">
          <KindIcon kind={entry.kind} />
        </span>
        {compact && !entry.identity ? null : (
          <span className={`quota-identity${loggedOut ? ' logged-out' : ''}`} aria-hidden="true">
            {entry.identity ?? entry.kind}
          </span>
        )}
        {/* 開關放左欄名稱下方，整格不會因它變高。 */}
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
        /* 手機風險不能只剩顏色（UI-DECISIONS：百分比始終保留），寫成數字。 */
        <span className="quota-compact">
          {windows.map((w) => {
            const soon = soonLabel(w.name, w.resetsAt, now)
            // 快回來了就寫倒數（2026-09-14／09-17 使用者）：5h 用完且三小時內；週窗口剩不到 10% 且 24 小時內。
            const back = resetBadge(w.pct, w.resetsAt, now, resetRule(w.name))
            return (
            <span key={`${w.group ?? ''}-${w.name}`} className={`quota-compact-win ${levelOf(w)}`}>
              <span className={`quota-window-name${soon ? ' soon' : ''}${w.staleAt ? ' carried' : ''}`} title={soon ? `${w.name} 還有 ${soon} 重置` : undefined}>
                {windowLabel(w, soon)}
                {w.staleAt ? <StaleWindowMark at={w.staleAt} name={w.name} now={now} /> : null}
              </span>
              {back ? (
                // 倒數時百分比也留著（2026-10-02 使用者：「雖然現在 7d 已經倒數，也要出現 %」），倒數在前。
                <>
                  <span className="quota-reset-at" title={`${w.pct !== null && w.pct > 0 ? `剩 ${fmtPct(w.pct)}%` : '用完了'}，還有 ${back} 重置`}>
                    {back}
                  </span>
                  <span className="quota-compact-pct">{w.pct === null ? '無資料' : `${fmtPct(w.pct)}%`}</span>
                </>
              ) : (
                <span className="quota-compact-pct">{w.pct === null ? '無資料' : `${fmtPct(w.pct)}%`}</span>
              )}
            </span>
            )
          })}
        </span>
      ) : (
      <span className={`quota-bars${windows.length === 1 ? ' single' : ''}`}>
        {windows.map((w) => {
          const span = WINDOW_MS[w.name]
          const mark = resetMark(w.resetsAt, span, now)
          const left = w.resetsAt ? new Date(w.resetsAt).getTime() - now : null
          const back = resetBadge(w.pct, w.resetsAt, now, resetRule(w.name))
          // 那一列已經寫倒數了，窗口名就別再換成剩幾分——同一個時間寫兩次，還會擠在一起。
          const soon = back ? null : soonLabel(w.name, w.resetsAt, now)
          return (
            <span key={`${w.group ?? ''}-${w.name}`} className="quota-window">
              <span className={`quota-window-name${soon ? ' soon' : ''}${w.staleAt ? ' carried' : ''}`} title={soon ? `${w.name} 還有 ${soon} 重置` : undefined}>
                {windowLabel(w, soon)}
                {w.staleAt ? <StaleWindowMark at={w.staleAt} name={w.name} now={now} /> : null}
              </span>
              <Bar
                pct={w.pct}
                low={w.low}
                critical={w.critical}
                mark={mark}
                markTitle={left === null ? undefined : `${w.name} 還有 ${fmtLeft(left)} 重置`}
                instead={
                  back ? (
                    <span className="quota-reset-at" title={`${w.pct !== null && w.pct > 0 ? `剩 ${fmtPct(w.pct)}%` : '用完了'}，還有 ${back} 重置`}>
                      {back}
                    </span>
                  ) : undefined
                }
              />
            </span>
          )
        })}
      </span>
      )}
      </button>
    </span>
  )
}

/** 暫時停用的自動解除時刻＝最近一次未到的 reset；null 表示只能手動解除。 */
function nextResetOf(q: KindQuota | null, now: number, q2: KindQuota | null = null): number | null {
  let next: number | null = null
  for (const w of [q?.five_hour, q?.seven_day, q?.fable, q2?.five_hour, q2?.seven_day, q2?.fable]) {
    if (!w?.resets_at) continue
    const t = Date.parse(w.resets_at)
    if (Number.isNaN(t) || t <= now) continue
    if (next === null || t < next) next = t
  }
  return next
}

/** popover 裡的暫時停用勾選格（見 docs/UI-DECISIONS.md）；卡片點擊由 `PopRow` 轉呼叫。 */
function DisableToggle({ on, label: name, onToggle }: { on: boolean; label: string; onToggle: () => void }) {
  return (
    <span className="quota-disable">
      <input
        type="checkbox"
        aria-label={`暫時停用 ${name}（底下的 Bot 先從側欄收起來，額度 reset 後自動回來）`}
        checked={on}
        onChange={onToggle}
      />
      <span className="quota-disable-note" aria-hidden="true">
        {on ? '已停用' : '停用'}
      </span>
    </span>
  )
}

/** 條上的停用開關（主要入口）；條子擠只留方塊，說明走 aria-label 與 `.icon-tip`。 */
function StripDisableToggle({ entry, host }: { entry: QuotaEntry; host: string }) {
  const q = useEntryQuota(entry, host)
  const disabledMap = useDisabledQuota()
  const key = quotaDisableKey(host, entry.kind, entry.identity)
  const off = isQuotaDisabled(disabledMap, key)
  const short = entryLabel(entry)
  const name = `${hostLabel(host)} · ${short}`
  return (
    <label
      className={`quota-cell-toggle icon-tip${off ? ' on' : ''}`}
      data-tip={off ? `${short} 已停用 · 額度 reset 後自動回來` : `停用 ${short} · 底下的 Bot 先收起來`}
    >
      <input
        type="checkbox"
        aria-label={
          off
            ? `解除停用 ${name}（底下的 Bot 回到側欄）`
            : `停用 ${name}（底下的 Bot 先從側欄收起來，額度 reset 後自動回來）`
        }
        checked={off}
        onChange={() => setQuotaDisabled(key, !off, off ? null : nextResetOf(q, Date.now()))}
      />
    </label>
  )
}

/** Codex 預設帳號開 shell 登入；命名身份走 daemon 展開該主機的設定。 */
function CodexShellLogin({ host, identity }: { host: string; identity: string | null }) {
  const command = useStore((s) => cliLoginCommand('codex', identityEnv(s, host, 'codex', identity)))
  if (shouldUseIdentityLogin(identity)) {
    return <IdentityCliLogin kind="codex" host={host} hostLabel={hostLabel(host)} identity={identity} />
  }
  return <QuotaLoginShell host={host} hostLabel={hostLabel(host)} kind="codex" command={command} />
}

function blockedLine(hit: QuotaLimitHit): string {
  const when = hit.until ? new Date(hit.until) : null
  const back = when && !Number.isNaN(when.getTime())
    ? `${when.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })} 才會恢復`
    : '恢復時間 CLI 沒寫，要等它下一回合跑得動'
  return `⛔ CLI 說這個帳號現在被擋住：${back}\n${hit.message}`
}

function PopLimitHit({ hit }: { hit: QuotaLimitHit | null | undefined }) {
  if (!hit) return null
  return (
    <div className="quota-pop-line limit-hit">
      <span className="quota-win">
        <span className="quota-ico" aria-hidden="true">
          ⛔
        </span>
        被擋
      </span>
      <span className="quota-limit-hit" title={hit.message}>
        {hit.until ? `${new Date(hit.until).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })} 恢復` : '等下一回合跑得動'}
        <span className="quota-limit-hit-why">CLI 回報額度上限，量表是速率視窗，看不到這件事</span>
      </span>
    </div>
  )
}

/** codex 額度重置券（2026-09-10 使用者）；daemon 只讀不用，要用在 codex `/status` → Reset usage。 */
function PopResetCredits({ credits, now }: { credits: QuotaResetCredits | null | undefined; now: number }) {
  if (!credits || credits.available <= 0) return null
  const left = credits.expires_at ? new Date(credits.expires_at).getTime() - now : null
  return (
    <div className="quota-pop-line reset-credits">
      <span className="quota-win">
        <span className="quota-ico" aria-hidden="true">
          ⟳
        </span>
        重置券
      </span>
      <span className="quota-reset-credit" title={`${credits.title ?? '額度重置券'}${left === null ? '' : `・${fmtLeft(left)}後過期`}・在 codex 裡用 /status → Reset usage`}>
        <strong>{credits.available}</strong> 張可用
        {credits.title ? <span className="quota-reset-title">{credits.title}</span> : null}
      </span>
    </div>
  )
}

/** 共用 `Bar`：手機點開也要看得到圖示化進度（2026-09-09 使用者）。 */
function PopWindow({
  icon,
  name,
  win,
  w,
  now,
}: {
  icon: string
  name: string
  win: WindowName
  w: QuotaWindow | null | undefined
  now: number
}) {
  const pct = remaining(w)
  if (pct === null) return null
  const left = w?.resets_at ? new Date(w.resets_at).getTime() - now : null
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
        markTitle={left === null ? undefined : `${name} 還有 ${fmtLeft(left)} 重置`}
      />
      {/* 寫「還有多久」不寫時刻（2026-09-09 使用者要求）；絕對時刻留在 tooltip。 */}
      <span className="quota-reset" title={w?.resets_at ? `重置時刻 ${fmtTime(w.resets_at)}` : undefined}>
        <span className="quota-ico" aria-hidden="true">
          ↻
        </span>
        {left === null ? '—' : fmtLeftLong(left)}
      </span>
    </div>
  )
}

function PopRow({ entry, host, onAgyLogout }: { entry: QuotaEntry; host: string; onAgyLogout: () => void }) {
  const q = useEntryQuota(entry, host)
  const q2 = useAgyClaudeGpt(entry, host)
  const now = useMinuteNow()
  const known = useStore((s) => {
    if (entry.identity && quotaKey(host, `${entry.kind}:${entry.identity}`) in s.quota) return true
    return entry.fullKey in s.quota || (entry.kind === 'agy' && quotaKey(host, `agy:${AGY_CLAUDE_GPT}`) in s.quota)
  })
  const supported = QUERYABLE.includes(entry.kind)
  const loggedOut = useLoggedOut(entry, host)
  const agyLogoutBusy = useStore((s) => s.busy[`agy-logout:${host || 'local'}`] === true)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day) ?? remaining(q2?.seven_day)
  const five2 = remaining(q2?.five_hour)
  const agyHasWindow = [five, remaining(q?.seven_day), five2, remaining(q2?.seven_day)].some((pct) => pct !== null)
  const disabledMap = useDisabledQuota()
  const key = quotaDisableKey(host, entry.kind, entry.identity)
  const off = isQuotaDisabled(disabledMap, key)
  const toggle = () => setQuotaDisabled(key, !off, off ? null : nextResetOf(q, Date.now(), entry.kind === 'agy' ? q2 : null))

  return (
    <div
      className={`quota-pop-row${off ? ' off' : ''}`}
      // 點卡片＝切換 checkbox；控制項自己的點擊放行，免得切兩下或吃掉登入鈕。
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
        {supported ? <RiskDot level={entry.kind === 'agy' ? worse(worst(q), worst(q2)) : worst(q)} /> : null}
        {q?.plan ? <span className="quota-plan">{q.plan}</span> : null}
        <DisableToggle on={off} label={entryLabel(entry)} onToggle={toggle} />
      </div>
      {q?.stale ? <p className="quota-stale-note">{staleSuffix(q, now)}，新的探測回來後會更新</p> : null}
      {!supported ? (
        <p className="quota-pop-note">CLI 不支援額度查詢</p>
      ) : !known || (entry.kind === 'agy' ? !agyHasWindow : five === null && seven === null) ? (
        <>
          <p className={`quota-pop-note${loggedOut ? ' warn' : ''}`}>
            {/* 登入偵測在該主機 herdr pane 裡跑、看得到 Keychain，所以「沒登入」可信。 */}
            {loggedOut
              ? `${hostLabel(host)} 上這個帳號未登入。`
              : weeklyOnlyKind(entry.kind)
                ? '背景查詢中'
                : `尚未取得（啟動一個 ${KIND_LABEL[entry.kind]} bot 後回報）`}
          </p>
          {loggedOut ? (
            entry.kind === 'codex' ? (
              <CodexShellLogin host={host} identity={entry.identity} />
            ) : (
              <QuotaLoginSlash kind={entry.kind} host={host} hostLabel={hostLabel(host)} identity={entry.identity} />
            )
          ) : null}
        </>
      ) : (
        entry.kind === 'agy' ? (
          <>
            <PopWindow icon="⏱" name={`${AGY_BUCKETS[0].name} · 5h`} win="5h" w={q?.five_hour} now={now} />
            <PopWindow icon="📅" name={`${AGY_BUCKETS[0].name} · 每週`} win="週" w={q?.seven_day} now={now} />
            <PopWindow icon="⏱" name={`${AGY_BUCKETS[1].name} · 5h`} win="5h" w={q2?.five_hour} now={now} />
            <PopWindow icon="📅" name={`${AGY_BUCKETS[1].name} · 每週`} win="週" w={q2?.seven_day} now={now} />
            <PopLimitHit hit={q?.limit_hit} />
            <PopLimitHit hit={q2?.limit_hit} />
          </>
        ) : (
        <>
          <PopWindow icon="⏱" name="5h" win="5h" w={q?.five_hour} now={now} />
          <PopWindow icon="📅" name={weekLabel(entry.kind)} win="7d" w={q?.seven_day} now={now} />
          <PopWindow icon="✦" name="Fable" win="F" w={q?.fable} now={now} />
          <PopLimitHit hit={q?.limit_hit} />
          <PopResetCredits credits={q?.reset_credits} now={now} />
        </>
        )
      )}
      {/* agy 沒有 CLI 登出，daemon 直接清憑證；已經未登入就沒東西可清。 */}
      {entry.kind === 'agy' && !loggedOut ? (
        <div className="quota-pop-actions">
          <AgyLogoutButton host={host} onOpen={onAgyLogout} busy={agyLogoutBusy} />
        </div>
      ) : null}
    </div>
  )
}

/** 顯示中的子節點（`display: none` 的漢堡鈕、spacer 不算，也不吃 gap）。 */
function shownChildren(el: Element): HTMLElement[] {
  return [...el.children].filter((c): c is HTMLElement => c instanceof HTMLElement && c.getClientRects().length > 0)
}

/**
 * 額度區能拿到的寬：標題列扣掉內距、gap 與其他東西。遠端記憶體（`.mem-wrap`）會自己縮到 0（收縮優先序在量表前面），
 * 所以它的寬算在可用裡；它的 gap 還是扣掉，縮到 0 時那 10px 照樣在。
 */
function quotaAvail(head: HTMLElement, strip: HTMLElement): number {
  const cs = getComputedStyle(head)
  const kids = shownChildren(head)
  let used = (parseFloat(cs.paddingLeft) || 0) + (parseFloat(cs.paddingRight) || 0) + (parseFloat(cs.columnGap) || 0) * Math.max(0, kids.length - 1)
  for (const k of kids) {
    if (k !== strip && !k.classList.contains('mem-wrap')) used += k.getBoundingClientRect().width
  }
  return head.clientWidth - used
}

/**
 * 額度列照內容排開要多寬：量第一格到最後一格的外框（不用 scrollWidth：停用方塊的提示是絕對定位，會把它撐大）。
 * 格子平常會 flex-grow 填滿，量的當下暫時關掉，量到的才是「需要」而不是「分到」；同步量完就還原，不會畫出來。
 */
function quotaContent(row: HTMLElement): number {
  const kids = shownChildren(row)
  if (kids.length === 0) return 0
  for (const k of kids) k.style.flexGrow = '0'
  const first = kids[0]
  const last = kids[kids.length - 1]
  const cs = getComputedStyle(row)
  const width =
    last.getBoundingClientRect().right - first.getBoundingClientRect().left
    + (parseFloat(getComputedStyle(first).marginLeft) || 0) + (parseFloat(getComputedStyle(last).marginRight) || 0)
    + (parseFloat(cs.paddingLeft) || 0) + (parseFloat(cs.paddingRight) || 0)
  for (const k of kids) k.style.flexGrow = ''
  return width
}

export function QuotaStrip({
  focusKind,
  focusIdentity,
  host = LOCAL_HOST,
}: {
  focusKind?: BotKind | null
  /** null = default / cc0 account. */
  focusIdentity?: string | null
  /** 顯示哪一台主機的額度（SPEC §14），預設本機。 */
  host?: string
}) {
  const quota = useStore((s) => s.quota)
  const configured = useStore((s) => s.identities)
  // 身份清單跟著該主機（SPEC §16）：遠端 cc1 可能是不同帳號。
  const idStatus = useStore((s) => identityStatusOfHost(s, host))
  const identities = useMemo(() => identitiesOfHost(configured, idStatus, host ?? 'local'), [configured, idStatus, host])
  // 桌機讓位階段（`lib/quotaLayout.ts`）：量標題列扣掉其他東西後的寬，不是 window.innerWidth（少算側欄與暫存欄約 500px，
  // 2026-09-11 分頁被推出畫面）。格子或主機一換就從頭量。
  const [fitState, setFitState] = useState<{ key: string; s: QuotaFitState }>({ key: '', s: QUOTA_FIT_START })
  const [open, setOpen] = useState(false)
  /** agy 登出的確認框；開關放在這層（見 `AgyLogout.tsx`）。 */
  const [agyLogoutOpen, setAgyLogoutOpen] = useState(false)
  // 手機點某一格只看那一格（2026-09-09 使用者）。
  const [only, setOnly] = useState<string | null>(null)
  const phone = useMediaQuery(PHONE_QUERY)
  // 手機明細貼在額度列正下方（2026-09-09 使用者：跳在底下太遠）。
  const [popTop, setPopTop] = useState<number | null>(null)
  const measure = () => {
    const r = wrap.current?.getBoundingClientRect()
    setPopTop(phone && r ? Math.round(r.bottom + 6) : null)
  }
  const wrap = useRef<HTMLDivElement>(null)

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

  useEffect(() => {
    if (!phone) return
    wrap.current?.querySelector('.quota-hp.focused')?.scrollIntoView({ inline: 'center', block: 'nearest' })
  }, [phone, focusKind, focusIdentity])

  const disabledIdentities = useStore((s) => s.disabledIdentities)
  // 停用的身份不上額度條：連它自己那把 `claude:<name>` 孤兒 key 也不列（使用者 2026-09-16）。
  const ordered = useMemo(() => {
    // 落點的認領清單跟側欄同一支（`quotaClaimantsOf`）。
    const live = quotaClaimantsOf(configured, idStatus, disabledIdentities, host)
    return collectEntries(quota, live, host).filter(
      (e) => !e.identity || !disabledIdentities.includes(identityPrefKey(host, e.kind, e.identity)),
    )
  }, [quota, configured, idStatus, host, disabledIdentities])

  const fitKey = `${host}|${ordered.map(entryReactKey).join(',')}`
  const fit = fitState.key === fitKey ? fitState.s : QUOTA_FIT_START

  // layout effect：首次繪製前量到並收斂（每換一階段就同步重畫、再量），不會先閃一次全部攤開或疊字。
  // 沒有任何額度時整條不畫（`wrap` 是 null）；deps 要跟著「有沒有畫」走，不然額度晚到時永遠不會開始量寬度。
  // 格子變寬（倒數字變長）、標題列或旁邊的分頁變寬都要重量，所以連同它們一起觀察。
  const drawn = ordered.length > 0
  useLayoutEffect(() => {
    const strip = wrap.current
    const head = strip?.parentElement
    const row = strip?.querySelector<HTMLElement>('.quota-open')
    if (!drawn || phone || !strip || !head || !row) return
    const remeasure = () => {
      const next = nextQuotaFit(fit, quotaAvail(head, strip), quotaContent(row), false)
      if (next !== fit) setFitState({ key: fitKey, s: next })
    }
    remeasure()
    if (typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(remeasure)
    for (const el of [head, ...head.children, ...row.children]) ro.observe(el)
    return () => ro.disconnect()
  }, [drawn, phone, fit, fitKey])

  const popEntries = ordered

  if (ordered.length === 0) return null

  const freshest = popEntries
    .map((e) => quota[e.fullKey]?.updated_at ?? null)
    .filter((x): x is string => Boolean(x))
    .sort()
    .pop()

  const collapsed = !phone && fit.level === 2
  /** CSS 也是 640px 那條線。 */
  const compact = phone
  // 全部帳號都畫完整量表（2026-09-11 使用者：「額度顯示是很重要的訊息，不要去省他的空間」）。
  const shown = ordered
  return (
    <div className="quota-strip" ref={wrap} aria-label={quotaTitle(host)}>
      {/* 容器而非 button：checkbox 不能塞在 button 裡。 */}
      <div className={`quota-open${collapsed ? ' collapsed' : ''}`}>
        {/* 更新 chip 放最左邊，避免夾在兩個 kind 間被誤認（2026-09-11 使用者）。手機搬到標題列 ★ 左邊（ChatPanel，2026-09-19 使用者）。 */}
        {phone ? null : <UpdateQuotaChip />}
        {/* 不掛主機名牌（2026-09-29 使用者）：標題列左邊的 `@host` 已經說了是哪台；tooltip／popover 標題仍寫主機。 */}
        {shown.map((entry) => {
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
                collapsed={collapsed || compact}
                compact={compact}
                focused={focused}
                open={open}
                onOpen={onOpen}
              />
            </Fragment>
          )
        })}
      </div>
      {open ? (
        <div className="quota-pop" role="dialog" aria-label={`所有${quotaTitle(host)}`} style={popTop !== null ? { top: popTop } : undefined}>
          <div className="quota-pop-title">{quotaTitle(host)}</div>
          {(only ? popEntries.filter((e) => entryReactKey(e) === only) : popEntries).map((entry) => (
            <PopRow key={entryReactKey(entry)} entry={entry} host={host} onAgyLogout={() => setAgyLogoutOpen(true)} />
          ))}
          {only && popEntries.length > 1 ? (
            <button type="button" className="mini-btn quota-pop-all" onClick={() => setOnly(null)}>
              看全部（{popEntries.length}）
            </button>
          ) : null}
          {freshest ? <p className="quota-pop-foot">更新於 {fmtTime(freshest)} · ↻ 是距離重置還有多久</p> : null}
        </div>
      ) : null}
      <AgyLogoutDialog host={host} open={agyLogoutOpen} onClose={() => setAgyLogoutOpen(false)} />
    </div>
  )
}
