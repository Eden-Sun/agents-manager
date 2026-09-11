import { Fragment, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, Identity, KindQuota, QuotaMap, QuotaResetCredits, QuotaWindow } from '../api/types'
import { LOCAL_HOST, quotaKey } from '../api/types'
import { identitiesOfHost, identityStatusOfHost, toolsOfHost, useStore } from '../store/store'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { isQuotaDisabled, quotaDisableKey, setQuotaDisabled, useDisabledQuota } from '../store/quotaHide'
import { KindIcon, KIND_LABEL } from './KindTag'
import { QuotaLoginShell } from './QuotaLoginShell'
import { QuotaLoginSlash } from './QuotaLoginSlash'
import { UpdateQuotaChip } from './UpdateQuotaChip'
import { cliLoginCommand, identityEnv } from '../lib/quotaLogin'

/**
 * Remaining quota per kind (`GET /api/quota` + WS `quota_updated`).
 *
 * Decision (Codex sol, docs/UI-DECISIONS.md follow-up): the strip keeps every kind that has
 * reported quota permanently visible; only the *windows* per kind collapse below 1100px, never
 * a whole kind. Drawn as frameless health bars: the fill length carries the level, colour only
 * reinforces it, so it survives greyscale. Full numbers live in the tooltip and popover.
 *
 * 桌機上**每個帳號都畫完整的量表**（2026-09-11 使用者：「額度顯示是很重要的訊息，不要去省
 * 他的空間」）：五個帳號各自身分名 + 5h／7d／F 三行，焦點那格只是多一圈外框，不是「只有它
 * 完整」。這條列佔掉 ~700px 是刻意的資訊密度，標題列放不下由標題列自己排收縮優先序解決，
 * 不從額度身上省，也不收進 `+N`。
 *
 * 窄視窗與手機的判斷（`collapsed`／`compact`）量的是**標題列**（`ResizeObserver` 掛在父節點），
 * 不是 `window.innerWidth`：後者少算了側欄與圖片暫存欄約 500px。
 *
 * The kinds do not report the same windows: claude and codex have both 5h and 7d, grok only a
 * weekly one (scraped from its `/usage` dialog, SPEC §12.6). A kind therefore draws one bar per
 * window it actually reports — never a filler bar for a window that does not exist.
 *
 * Claude identities (`cc0`, `cc1`, …) each get their own gauge on the strip (icon + identity
 * label). The bare `claude` quota key is the default account — when a `cc0` (or empty-env)
 * identity exists it is shown as that label, not as an unlabeled Claude row.
 *
 * 額度是按主機分開的（SPEC §14）。這條列一次只顯示**一台**主機：預設本機，看的是
 * 遠端 bot／專案時就換成那台，並在最左邊掛一個主機名稱標籤（本機不掛，維持原樣）。
 * store 裡遠端的 key 帶 `<host>/` 前綴，這裡先投影成裸 key 再跑原本那套排序規則。
 */

/** Every kind can report quota; grok arrives from the `/usage` probe. */
const QUERYABLE: BotKind[] = ['claude', 'codex', 'grok']

/** One strip / popover row: base kind or `kind:identity`, plus the host-scoped map key. */
type QuotaEntry = { key: string; fullKey: string; kind: BotKind; identity: string | null }

/** 只留下屬於 `host` 的額度，並把 key 還原成裸的（`m4p/claude:cc1` → `claude:cc1`）。 */
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

/** 這台主機在 UI 上的名字。 */
function hostLabel(host: string): string {
  return host === LOCAL_HOST ? '本機' : host
}

/** 標題／aria 用的說法：「本機額度」對上「m4p 的額度」。 */
function quotaTitle(host: string): string {
  return host === LOCAL_HOST ? '本機額度' : `${host} 的額度`
}

/** 一列額度對應的 store 值：身份專屬的 key 優先，沒有才退回這列自己的 key。 */
function useEntryQuota(entry: QuotaEntry, host: string): KindQuota | null {
  return useStore((s) => {
    if (entry.identity) {
      const keyed = quotaKey(host, `${entry.kind}:${entry.identity}`)
      if (s.quota[keyed] != null) return s.quota[keyed]
    }
    return s.quota[entry.fullKey] ?? null
  })
}

/**
 * 這個身份在**這台**主機上有沒有被判定為沒登入。
 *
 * cc1 在本機是一個帳號、在 m4p 可能根本沒登入過，所以要看那台 host 的 `identity_status`，
 * 不是全域那份 `[[identities]]`。條子（`Gauge`）和 popover（`PopRow`）講的是同一件事，
 * 兩邊共用這一支——各寫一份的話，判斷會慢慢漂走。
 */
function useLoggedOut(entry: QuotaEntry, host: string): boolean {
  return useStore((s) => {
    const name = entry.identity
    // 預設帳號（沒有身份名）看的是 `tools.<kind>.logged_in`：grok / codex 沒登入時一直寫
    // 「背景查詢中」，其實是永遠查不到。
    if (!name) return toolsOfHost(s, host)[entry.kind]?.logged_in === false
    return identityStatusOfHost(s, host)[name]?.logged_in === false
  })
}

type Level = 'crit' | 'warn' | 'ok'

/**
 * 剩餘百分比。10 以上取整數；**不到 10 時留一位小數**（2026-09-08）：快用完的時候 9.8 和 9.1
 * 差一整回合，四捨五入成 10 反而讓人以為還有餘裕。
 */
function remaining(w: QuotaWindow | null | undefined): number | null {
  if (!w) return null
  const left = Math.max(0, 100 - w.used_pct)
  return left < 10 ? Math.round(left * 10) / 10 : Math.round(left)
}

/** `remaining` 的顯示字：不到 10 且有小數才帶一位（`9.8`）；`9.0` 就是 `9`，10 以上整數。 */
function fmtPct(pct: number): string {
  return String(pct)
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
  const windows = [q?.five_hour, q?.seven_day, q?.fable].filter((w): w is QuotaWindow => w != null)
  if (windows.some((w) => w.critical)) return 'crit'
  if (windows.some((w) => w.low)) return 'warn'
  return 'ok'
}

/** The window that is closest to running out — what the collapsed pill shows. */
function worstWindow(q: KindQuota | null): { name: WindowName; pct: number | null } {
  const cands: { name: WindowName; pct: number }[] = []
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const fable = remaining(q?.fable)
  if (five !== null) cands.push({ name: '5h', pct: five })
  if (seven !== null) cands.push({ name: '7d', pct: seven })
  // 沒有 Fable 桶就不進來，收合的膠囊也不會憑空多一個窗口。
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

function entryLabel(entry: QuotaEntry): string {
  return entry.identity ? `${KIND_LABEL[entry.kind]} · ${entry.identity}` : KIND_LABEL[entry.kind]
}

function label(entry: QuotaEntry, q: KindQuota | null, loggedOut = false): string {
  const parts = [entryLabel(entry)]
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  if (five === null && seven === null) parts.push(loggedOut ? '這台主機偵測不到登入，額度尚未取得' : '額度尚未取得')
  if (five !== null) parts.push(`5 小時剩餘 ${five}%`)
  if (seven !== null) {
    parts.push(entry.kind === 'grok' ? `每週剩餘 ${seven}%` : `7 天剩餘 ${seven}%`)
  }
  const fable = remaining(q?.fable)
  if (fable !== null) parts.push(`Fable 每週剩餘 ${fable}%`)
  if (q?.five_hour?.resets_at) parts.push(`5 小時 ${fmtTime(q.five_hour.resets_at)} 重置`)
  if (q?.seven_day?.resets_at) {
    parts.push(
      entry.kind === 'grok'
        ? `每週 ${fmtTime(q.seven_day.resets_at)} 重置`
        : `7 天 ${fmtTime(q.seven_day.resets_at)} 重置`,
    )
  }
  if (q?.fable?.resets_at) parts.push(`Fable ${fmtTime(q.fable.resets_at)} 重置`)
  return parts.join('，')
}

/** Parse `claude` / `claude:cc1` into a strip entry; unknown kinds are ignored. */
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

function entryReactKey(entry: Omit<QuotaEntry, 'fullKey'>): string {
  return entry.identity ? `${entry.kind}:${entry.identity}` : entry.key
}

/**
 * Strip / popover entries in a **fixed** order — never by remaining %:
 *   cc0 → cc1 → (other claude identities) → codex → grok
 * When no Claude identities are configured, bare `claude` stands in for the Claude slot.
 *
 * `host` 決定看哪一台的數字：先把 map 投影成該主機的裸 key，最後再把 `fullKey` 補回去，
 * 所以底下這套排序規則完全不必知道主機的存在。
 */
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

/** 額度條上的窗口名稱：`F` 是 Max 方案的 Fable 週窗，只有 claude 有。 */
type WindowName = '5h' | '7d' | '週' | 'F'

/** 每個窗口有多長：位置刻度就是拿「離重置還有多久」去除這個。 */
const WINDOW_MS: Record<WindowName, number> = {
  '5h': 5 * 3_600_000,
  '7d': 7 * 86_400_000,
  '週': 7 * 86_400_000,
  // Fable 也是週窗，刻度跟 7d 同一把尺。
  F: 7 * 86_400_000,
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

/**
 * 彈出層那一列用的「還剩多久」：`2 天 3 小時` / `4 小時 20 分` / `12 分`。
 *
 * 跟 [`fmtLeft`] 是同一個數字，只是那個是給 tooltip 與窄處用的緊縮寫法（`2d3h`）。這裡有位置，
 * 就寫成讀得出口的樣子——這一列的主角是「還能撐多久」。
 */
function fmtLeftLong(ms: number): string {
  if (ms <= 0) return '即將重置'
  const m = Math.floor(ms / 60_000)
  const h = Math.floor(m / 60)
  const d = Math.floor(h / 24)
  if (d > 0) return `${d} 天 ${h % 24} 小時`
  if (h > 0) return `${h} 小時 ${m % 60} 分`
  return `${m} 分`
}

/**
 * 5h 窗口不到一小時就要重置時，窗口名直接換成剩幾分（`12m`）：使用者要能提早準備
 * （2026-09-09）。回 null 表示照常寫 `5h`。只做 5h——7d／週的重置沒有「等一下就回來」的意義。
 */
function soonLabel(name: WindowName, resetsAt: string | null, now: number): string | null {
  if (name !== '5h' || !resetsAt) return null
  const left = new Date(resetsAt).getTime() - now
  if (Number.isNaN(left) || left >= 60 * 60_000) return null
  return left <= 0 ? '0m' : `${Math.max(1, Math.ceil(left / 60_000))}m`
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
      {/* 數字一律印（UI-DECISIONS：百分比始終保留）：以前只有 low 才印，健康的那行沒有數字，
          三行右緣參差不齊（2026-09-09 使用者截圖）。健康的用淡色，黃／紅照舊。 */}
      <span className={`quota-bar-pct ${lv}${pct === null ? ' nodata' : ''}`} aria-hidden="true">
        {pct === null ? '—' : fmtPct(pct)}
      </span>
    </span>
  )
}

type WindowBar = { name: WindowName; pct: number | null; resetsAt: string | null; low: boolean; critical: boolean }

/** grok only reports a weekly window (stored in seven_day) — never call it 7d. */
function weekLabel(kind: BotKind): '7d' | '週' {
  return kind === 'grok' ? '週' : '7d'
}

/** Frameless, compact: icon above identity, bars to the right — keeps label glued to its bars. */
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
  /** 手機：條子縮成一顆 chip，剩餘量改用數字寫出來（見 `QuotaStrip` 的 `compact`）。 */
  compact: boolean
  focused: boolean
  /** popover 開著沒（給 `aria-expanded`）。 */
  open: boolean
  onOpen: () => void
}) {
  const q = useEntryQuota(entry, host)
  const loggedOut = useLoggedOut(entry, host)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  // 上下兩條邊框量表**只有手機畫**（2026-09-11 使用者）：桌機同一格裡已經有 5h／7d 的 bar
  // 與數字，邊框是同一份資訊再畫一次；手機的量表被壓成純文字 chip，看不到 bar，才需要它。
  const borderWindows = focused && compact ? [
    { edge: 'top', label: '5H', window: q?.five_hour, pct: five },
    { edge: 'bottom', label: weekLabel(entry.kind).toUpperCase(), window: q?.seven_day, pct: seven },
  ].filter((w) => w.pct !== null && Number.isFinite(w.pct)) : []
  const fable = remaining(q?.fable)
  const now = useMinuteNow()
  const disabledMap = useDisabledQuota()
  // Named windows so 5h stays above 7d/週; collapsed shows only the worst.
  let windows: WindowBar[]
  // 手機：**7d 常駐**（grok 是「週」）＋任何一個在警戒中的其他窗口。
  //
  // 7d 是決定「今天還能不能開工」的數字，5h 兩三個小時就回來了，所以它固定在同一個位置、
  // 不會被別的窗口擠掉；但「5h 只剩 3%」是現在就會擋住你的事，不能等到點開才知道。所以是
  // 常駐一個＋例外才追加，而不是收成最差的一個（7d 會被蓋掉）或三個全列（一排捲不完）。
  if (compact && seven !== null) {
    windows = [
      {
        name: weekLabel(entry.kind),
        pct: seven,
        resetsAt: q?.seven_day?.resets_at ?? null,
        low: q?.seven_day?.low ?? false,
        critical: q?.seven_day?.critical ?? false,
      },
    ]
    // 5h / Fable 只有在 daemon 標成 low／critical 時才佔位（門檻見 docs/API.md §12.4）。
    if (five !== null && (q?.five_hour?.low || q?.five_hour?.critical)) {
      windows.push({
        name: '5h',
        pct: five,
        resetsAt: q?.five_hour?.resets_at ?? null,
        low: q?.five_hour?.low ?? false,
        critical: q?.five_hour?.critical ?? false,
      })
    }
    if (fable !== null && (q?.fable?.low || q?.fable?.critical)) {
      windows.push({
        name: 'F',
        pct: fable,
        resetsAt: q?.fable?.resets_at ?? null,
        low: q?.fable?.low ?? false,
        critical: q?.fable?.critical ?? false,
      })
    }
  } else if (collapsed) {
    const w = worstWindow(q)
    const src = w.name === '5h' ? q?.five_hour : w.name === 'F' ? q?.fable : q?.seven_day
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
    // Max 方案的 Fable 週額度：daemon 有回報才多這一條，沒有就不畫也不佔位。
    if (fable !== null) {
      windows.push({
        name: 'F',
        pct: fable,
        resetsAt: q?.fable?.resets_at ?? null,
        low: q?.fable?.low ?? false,
        critical: q?.fable?.critical ?? false,
      })
    }
  }
  // 主機名寫進 tooltip：條上只掛得下一個小標籤，但滑過去要能確定是哪一台的數字。
  const title = `${hostLabel(host)} · ${label(entry, q, loggedOut)}`
  // 停用中的那一格在條上也要看得出來，不然得先點開 popover 才知道側欄少了誰。
  const off = isQuotaDisabled(disabledMap, quotaDisableKey(host, entry.kind, entry.identity))
  const withOff = off ? `${title}（已暫時停用，底下的 Bot 收在側欄外）` : title
  const accessibleTitle = focused
    ? `目前選取的 ${withOff}${borderWindows.map((w) => `；${w.edge === 'top' ? '上' : '下'}邊框：${w.label} 剩餘 ${fmtPct(w.pct!)}%`).join('')}`
    : withOff

  return (
    <span
      className={`quota-hp ${entry.kind} ${worst(q)}${focused ? ' focused' : ''}${borderWindows.length ? ' quota-framed' : ''}${off ? ' off' : ''}`}
      title={accessibleTitle}
      aria-current={focused ? 'true' : undefined}
      // 整格可點開 popover。從停用方塊或量表按鈕發出的點擊放行——量表那顆自己會處理，
      // 不放行就會一次開一次關。
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
        {/* claude 才有身分名（cc0 / cc1 …）；codex、grok 就寫 kind 自己的名字。三種 kind 的左欄
            因此都是「圖示 / 名稱 / 開關」三層，開關一律貼在名稱正下方，不會有一格歪掉。 */}
        {compact && !entry.identity ? null : (
          <span className={`quota-identity${loggedOut ? ' logged-out' : ''}`} aria-hidden="true">
            {entry.identity ?? entry.kind}
          </span>
        )}
        {/* 停用開關跟圖示／名稱同一直欄，貼在名稱正下方：右邊那幾條進度條的高度不變，
            整格也就不會因為它變高。 */}
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
        /* 手機上一條 38px 的量表比它旁邊的所有東西都不重要，但風險不能只剩顏色
           （UI-DECISIONS：百分比始終保留），所以把最吃緊的那個窗口寫成數字。 */
        <span className="quota-compact">
          {windows.map((w) => {
            const soon = soonLabel(w.name, w.resetsAt, now)
            return (
            <span key={w.name} className={`quota-compact-win ${levelOf(w)}`}>
              <span className={`quota-window-name${soon ? ' soon' : ''}`} title={soon ? `5h 還有 ${soon} 重置` : undefined}>{soon ?? w.name}</span>
              <span className="quota-compact-pct">{w.pct === null ? '無資料' : `${fmtPct(w.pct)}%`}</span>
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
          const soon = soonLabel(w.name, w.resetsAt, now)
          return (
            <span key={w.name} className="quota-window">
              <span className={`quota-window-name${soon ? ' soon' : ''}`} title={soon ? `5h 還有 ${soon} 重置` : undefined}>
                {soon ?? w.name}
              </span>
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
      )}
      </button>
    </span>
  )
}

/**
 * 這一格「暫時停用」時，什麼時候自動解除——這組額度最近一次還沒到的 reset 時刻。
 * 三個視窗都沒有時間就回 null（那格只能手動解除）。
 */
function nextResetOf(q: KindQuota | null, now: number): number | null {
  let next: number | null = null
  for (const w of [q?.five_hour, q?.seven_day, q?.fable]) {
    if (!w?.resets_at) continue
    const t = Date.parse(w.resets_at)
    if (Number.isNaN(t) || t <= now) continue
    if (next === null || t < next) next = t
  }
  return next
}

/**
 * 「暫時停用這個身分」的勾選格。額度快用完時勾起來，它底下的 bot 就先從側欄收起來，
 * 額度視窗 reset 到了自動解除（見 docs/UI-DECISIONS.md）。
 *
 * 卡片整格可點：`PopRow` 的 `onClick` 會轉呼叫這裡，所以這顆 input 只要管自己的鍵盤與
 * 勾選語意——`aria-label` 講完整句，卡片上的字只是提示。
 */
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

/**
 * 條上的停用開關——主要入口就在這裡，貼在 kind 圖示／身分名底下的同一直欄。
 * 條子很擠，所以只留方塊本身：說明走 `aria-label`，滑過去用既有的 `.icon-tip` 泡泡。
 * popover 裡那份（`DisableToggle`）留著當詳細版，兩邊共用同一份狀態。
 */
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

/** codex 沒有 `/login`，一律開 shell 跑 `codex login`。 */
function CodexShellLogin({ host, identity }: { host: string; identity: string | null }) {
  const command = useStore((s) => cliLoginCommand('codex', identityEnv(s, host, 'codex', identity)))
  return <QuotaLoginShell host={host} hostLabel={hostLabel(host)} kind="codex" command={command} />
}

/**
 * 彈出層裡的一條窗口：`⏱ 5h ▐▇▇▇░░ 20% ↻ 11:00`。
 *
 * 條子跟 reset 的黑針跟條上那顆量表用**同一個** `Bar`（2026-09-09 使用者要求：手機點開額度
 * 要看得到跟電腦一樣的圖示化進度）。手機的條子是純文字 chip（38px 的量表在那裡沒有意義），
 * 所以「還剩多少 / 什麼時候回來」的圖形版本只剩這裡能看——那就不能只有數字。
 */
/**
 * codex 的「額度重置券」（`rateLimitResetCredits`，2026-09-10 使用者要求接進來）。
 *
 * 兩條桶子回答「什麼時候自己回血」，這一行回答另一件事：「你現在就能把它清掉，還有幾張、
 * 那張什麼時候過期」。額度歸零的當下那是唯一還能做的動作，所以它跟桶子並排、不是藏在別處。
 * daemon 只讀不用：真的要用還是在 codex 那邊（`/status` → `Reset usage`），這裡不代按。
 */
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
      {/* 「還有多久」而不是「幾號幾點」（2026-09-09 使用者要求）：讀的人問的是「還能撐多久」，
          日期要自己跟現在相減才知道答案，而黑針畫的本來就是這段剩餘時間。絕對時刻留在
          tooltip，跨日或要跟別人約時間時才需要。 */}
      <span className="quota-reset" title={w?.resets_at ? `重置時刻 ${fmtTime(w.resets_at)}` : undefined}>
        <span className="quota-ico" aria-hidden="true">
          ↻
        </span>
        {left === null ? '—' : fmtLeftLong(left)}
      </span>
    </div>
  )
}

function PopRow({ entry, host }: { entry: QuotaEntry; host: string }) {
  const q = useEntryQuota(entry, host)
  const now = useMinuteNow()
  const known = useStore((s) => {
    if (entry.identity && quotaKey(host, `${entry.kind}:${entry.identity}`) in s.quota) return true
    return entry.fullKey in s.quota
  })
  const supported = QUERYABLE.includes(entry.kind)
  const loggedOut = useLoggedOut(entry, host)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const disabledMap = useDisabledQuota()
  const key = quotaDisableKey(host, entry.kind, entry.identity)
  const off = isQuotaDisabled(disabledMap, key)
  const toggle = () => setQuotaDisabled(key, !off, off ? null : nextResetOf(q, Date.now()))

  return (
    <div
      className={`quota-pop-row${off ? ' off' : ''}`}
      // 整格可點：點卡片本身就等於切換那顆 checkbox。從 checkbox 自己或登入鈕發出來的
      // 點擊要放行，不然會一次切換兩下／順手把登入按鈕吃掉。
      onClick={(e) => {
        if ((e.target as HTMLElement).closest('input, button, a, select, textarea')) return
        toggle()
      }}
    >
      <div className="quota-pop-head">
        <span className="quota-kind" aria-hidden="true">
          <KindIcon kind={entry.kind} />
        </span>
        <span className="quota-name">{entryLabel(entry)}</span>
        {supported ? <RiskDot level={worst(q)} /> : null}
        {q?.plan ? <span className="quota-plan">{q.plan}</span> : null}
        <DisableToggle on={off} label={entryLabel(entry)} onToggle={toggle} />
      </div>
      {!supported ? (
        <p className="quota-pop-note">CLI 不支援額度查詢</p>
      ) : !known || (five === null && seven === null) ? (
        <>
          <p className={`quota-pop-note${loggedOut ? ' warn' : ''}`}>
            {/* 登入偵測改在該主機的 herdr pane 裡跑（claude 是 `claude auth status --json`），
                看得到 Keychain，所以這裡的「沒登入」就是真的沒登入，直接叫使用者去登入。 */}
            {loggedOut
              ? // 後半句「請去 Bot 設定按登入」由下面那顆按鈕取代：claude / grok 直接送 `/login`，
                // codex 沒有 `/login`，改開 shell 跑 `codex login`。
                `${hostLabel(host)} 上這個帳號未登入。`
              : entry.kind === 'grok'
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
        <>
          <PopWindow icon="⏱" name="5h" win="5h" w={q?.five_hour} now={now} />
          <PopWindow icon="📅" name={weekLabel(entry.kind)} win="7d" w={q?.seven_day} now={now} />
          <PopWindow icon="✦" name="Fable" win="F" w={q?.fable} now={now} />
          <PopResetCredits credits={q?.reset_credits} now={now} />
        </>
      )}
    </div>
  )
}

export function QuotaStrip({
  focusKind,
  focusIdentity,
  host = LOCAL_HOST,
}: {
  focusKind?: BotKind | null
  /** Selected bot's identity; null = default / cc0 account. */
  focusIdentity?: string | null
  /**
   * 要顯示哪一台主機的額度（SPEC §14）。預設本機；選到 ssh 主機上的 bot／專案時
   * 傳它的 host，數字就換成 daemon 從那台讀回來的。
   */
  host?: string
}) {
  const quota = useStore((s) => s.quota)
  const configured = useStore((s) => s.identities)
  // 這台主機認得的身份：config 的加上它 shell 裡的 `ccN`（SPEC §16）。遠端的 cc1 可能指到
  // 跟本機不同的帳號，額度列本來就一次只看一台，所以身份清單也要跟著那一台。
  const idStatus = useStore((s) => identityStatusOfHost(s, host))
  const identities = useMemo(() => identitiesOfHost(configured, idStatus), [configured, idStatus])
  /**
   * 這條列**實際拿得到的寬度**，不是視窗寬。
   *
   * 原本用 `window.innerWidth` 比 1500：可是標題列的寬度是「視窗 − 側欄 − 圖片暫存欄」，
   * 大約少 500px。於是視窗一過 1500，量表就從「只留焦點那格」變成全部攤開（696px），
   * 而標題列其實只有 ~970px——排在最後的「對話／終端」分頁被推出畫面右緣
   * （2026-09-11 於 1500–1555px 量到，正好是最常見的筆電寬度）。改成量自己的容器，
   * 側欄開關、暫存欄多寬、視窗多大都判斷得對。
   *
   * 量的是**父節點**（標題列）而不是 `wrap` 自己：`.quota-strip` 是 `flex: none`，寬度由
   * 內容決定，拿它回頭決定要畫幾格會來回震盪。標題列的寬度由版面給，不受這條列影響。
   */
  const [avail, setAvail] = useState<number | null>(null)
  const [open, setOpen] = useState(false)
  // 手機點某一格只看那一格（2026-09-09 使用者）：記下是哪一格開的；`+N` 與桌面仍看全部。
  const [only, setOnly] = useState<string | null>(null)
  const phone = useMediaQuery(PHONE_QUERY)
  // 手機的明細貼在額度列正下方、靠上對齊（2026-09-09 使用者：跳在底下太遠）。
  const [popTop, setPopTop] = useState<number | null>(null)
  const measure = () => {
    const r = wrap.current?.getBoundingClientRect()
    setPopTop(phone && r ? Math.round(r.bottom + 6) : null)
  }
  const wrap = useRef<HTMLDivElement>(null)

  // `useLayoutEffect`：第一次繪製前就量到，不然開頁會先閃一次「全部攤開」再收回去。
  useLayoutEffect(() => {
    const box = wrap.current?.parentElement
    if (!box) return
    setAvail(box.clientWidth)
    if (typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => setAvail(box.clientWidth))
    ro.observe(box)
    return () => ro.disconnect()
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

  useEffect(() => {
    if (!phone) return
    wrap.current?.querySelector('.quota-hp.focused')?.scrollIntoView({ inline: 'center', block: 'nearest' })
  }, [phone, focusKind, focusIdentity])

  /** Fixed: cc0 → cc1 → codex → grok. No remaining-% / focus reshuffle. */
  const ordered = useMemo(() => collectEntries(quota, identities, host), [quota, identities, host])

  /** Same fixed order as the strip (Claude identities expanded). */
  const popEntries = useMemo(() => collectEntries(quota, identities, host), [quota, identities, host])

  if (ordered.length === 0) return null

  const remote = host !== LOCAL_HOST
  const freshest = popEntries
    .map((e) => quota[e.fullKey]?.updated_at ?? null)
    .filter((x): x is string => Boolean(x))
    .sort()
    .pop()

  /** 還沒量到（第一次繪製、或沒有 ResizeObserver）就當作桌機寬度。 */
  const box = avail ?? 1416
  /**
   * Both queryable kinds always stay on the bar; only the per-kind windows collapse.
   *
   * 門檻量的是標題列（見 `avail`），不是視窗：`window.innerWidth` 少算了側欄與圖片暫存欄
   * 約 500px，同一個視窗寬在開／關側欄時給額度的空間差很多。
   */
  const collapsed = box < 604
  /** 手機：標題列連一顆量表都放不下，剩餘量改用數字寫在 chip 上。CSS 也是 640px 那條線。 */
  const compact = phone
  /**
   * 每一個帳號都在條上，而且都是完整的量表——**不收進 `+N`、也不縮成窄格**
   * （2026-09-11 使用者：「額度顯示是很重要的訊息，不要去省他的空間」）。
   * 標題列排不下時由標題列自己讓位（名字、pane chip、分頁鍵的收縮優先序），不從額度身上省。
   */
  const shown = ordered
  return (
    <div className="quota-strip" ref={wrap} aria-label={quotaTitle(host)}>
      {/* 這排本來整個是一顆 `<button>`。停用開關要長在每一格身分卡裡（數字正下方），
          checkbox 不能塞在 button 裡，所以改成一個容器：每一格自己有「點開 popover」的
          按鈕，開關是它的兄弟節點。點條子照樣打開 popover，行為沒變。 */}
      <div className={`quota-open${collapsed ? ' collapsed' : ''}`}>
        {/* 「claude 有更新」放在整條額度的**最左邊**（2026-09-11 使用者）。本來貼在 claude
            那幾格右邊，於是它夾在兩個 kind 中間，看起來像是後面那個 kind 的東西；靠左先出現
            就沒有這個誤會，位置也不會隨著窄視窗少畫幾格而跳來跳去。沒有更新時它自己不畫。 */}
        <UpdateQuotaChip />
        {/* 遠端才掛主機名：本機是預設狀態，多一個「本機」標籤只會佔掉標題列的寬度。 */}
        {remote ? (
          <span className="quota-host" aria-hidden="true">
            {host}
          </span>
        ) : null}
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
            <PopRow key={entryReactKey(entry)} entry={entry} host={host} />
          ))}
          {only && popEntries.length > 1 ? (
            <button type="button" className="mini-btn quota-pop-all" onClick={() => setOnly(null)}>
              看全部（{popEntries.length}）
            </button>
          ) : null}
          {/* 一行就夠：每個帳號各印一次「更新於」時，那幾個時間差不到一分鐘。 */}
          {freshest ? <p className="quota-pop-foot">更新於 {fmtTime(freshest)} · ↻ 是距離重置還有多久</p> : null}
        </div>
      ) : null}
    </div>
  )
}
