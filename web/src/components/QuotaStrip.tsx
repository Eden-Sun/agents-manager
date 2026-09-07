import { useEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, Identity, KindQuota, QuotaMap, QuotaWindow } from '../api/types'
import { LOCAL_HOST, quotaKey } from '../api/types'
import { identitiesOfHost, identityStatusOfHost, useStore } from '../store/store'
import { KindIcon, KIND_LABEL } from './KindTag'
import { QuotaLoginCodex } from './QuotaLogin-codex'
import { QuotaLoginSlash } from './QuotaLoginSlash'

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
    if (!name) return false
    return identityStatusOfHost(s, host)[name]?.logged_in === false
  })
}

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

/** `5h 81%` / `5h —`; never wraps, always the same shape. */
function pctText(pct: number | null): string {
  return pct === null ? '—' : `${pct}%`
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
}: {
  entry: QuotaEntry
  host: string
  collapsed: boolean
  /** 手機：條子縮成一顆 chip，剩餘量改用數字寫出來（見 `QuotaStrip` 的 `compact`）。 */
  compact: boolean
  focused: boolean
}) {
  const q = useEntryQuota(entry, host)
  const loggedOut = useLoggedOut(entry, host)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const fable = remaining(q?.fable)
  const now = useMinuteNow()
  // Named windows so 5h stays above 7d/週; collapsed shows only the worst.
  let windows: WindowBar[]
  if (collapsed) {
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
        {entry.identity ? (
          <span className={`quota-identity${loggedOut ? ' logged-out' : ''}`}>{entry.identity}</span>
        ) : null}
      </span>
      {compact ? (
        /* 手機上一條 38px 的量表比它旁邊的所有東西都不重要，但風險不能只剩顏色
           （UI-DECISIONS：百分比始終保留），所以把最吃緊的那個窗口寫成數字。 */
        <span className="quota-compact">
          <span className="quota-window-name">{windows[0].name}</span>
          <span className="quota-compact-pct">{windows[0].pct === null ? '無資料' : `${windows[0].pct}%`}</span>
        </span>
      ) : (
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
      )}
    </span>
  )
}

function PopRow({ entry, host }: { entry: QuotaEntry; host: string }) {
  const q = useEntryQuota(entry, host)
  const known = useStore((s) => {
    if (entry.identity && quotaKey(host, `${entry.kind}:${entry.identity}`) in s.quota) return true
    return entry.fullKey in s.quota
  })
  const supported = QUERYABLE.includes(entry.kind)
  const loggedOut = useLoggedOut(entry, host)
  const five = remaining(q?.five_hour)
  const seven = remaining(q?.seven_day)
  const fable = remaining(q?.fable)

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
              <QuotaLoginCodex host={host} hostLabel={hostLabel(host)} identity={entry.identity} />
            ) : (
              <QuotaLoginSlash kind={entry.kind} host={host} identity={entry.identity} />
            )
          ) : null}
        </>
      ) : (
        <>
          {five !== null ? (
            <div className="quota-pop-line">
              <span className="quota-win">5h</span>
              <span className={`quota-row ${levelOf(q?.five_hour)}`}>剩 {pctText(five)}</span>
              <span className="quota-reset">{fmtTime(q?.five_hour?.resets_at)}</span>
            </div>
          ) : null}
          {seven !== null ? (
            <div className="quota-pop-line">
              <span className="quota-win">{entry.kind === 'grok' ? '週' : '7d'}</span>
              <span className={`quota-row ${levelOf(q?.seven_day)}`}>剩 {pctText(seven)}</span>
              <span className="quota-reset">{fmtTime(q?.seven_day?.resets_at)}</span>
            </div>
          ) : null}
          {fable !== null ? (
            <div className="quota-pop-line">
              <span className="quota-win">Fable</span>
              <span className={`quota-row ${levelOf(q?.fable)}`}>剩 {pctText(fable)}</span>
              <span className="quota-reset">{fmtTime(q?.fable?.resets_at)}</span>
            </div>
          ) : null}
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

  // Both queryable kinds always stay on the bar; only the per-kind windows collapse.
  const collapsed = width < 1100
  /**
   * Under ~1500px the strip cannot hold five gauges *and* leave the title readable: measured
   * at 1200px it took 563px of a 912px header, which squeezed the team's issue title down to
   * its 84px floor and pushed the action buttons past the edge. When it is that tight only
   * the gauge for what you are looking at stays on the bar; the rest are one click away in
   * the popover, which lists every one of them anyway.
   */
  const tight = width < 1500
  /** 手機：標題列連一顆量表都放不下，剩餘量改用數字寫在 chip 上。 */
  const compact = width <= 640

  /** The one gauge worth the width when space is tight: the kind/identity this view is about. */
  const focusEntry = focusKind
    ? (ordered.find((e) => {
        if (e.kind !== focusKind) return false
        if (e.kind !== 'claude') return true
        const id = focusIdentity?.trim() || (claudeIdentities(identities).some((i) => i.name === 'cc0') ? 'cc0' : null)
        return id ? e.identity === id : !e.identity
      }) ?? null)
    : null
  const shown = tight ? [focusEntry ?? ordered[0]] : ordered
  const hidden = ordered.length - shown.length

  return (
    <div className="quota-strip" ref={wrap} aria-label={quotaTitle(host)}>
      <button
        type="button"
        className={`quota-open${collapsed ? ' collapsed' : ''}`}
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-label={`所有${quotaTitle(host)}`}
        title={`所有${quotaTitle(host)}`}
        onClick={() => setOpen((v) => !v)}
      >
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
          return (
            <Gauge key={entryReactKey(entry)} entry={entry} host={host} collapsed={collapsed} compact={compact} focused={focused} />
          )
        })}
        {hidden > 0 ? (
          <span className="quota-more" title={`還有 ${hidden} 組額度，點開看`}>
            +{hidden}
          </span>
        ) : null}
      </button>
      {open ? (
        <div className="quota-pop" role="dialog" aria-label={`所有${quotaTitle(host)}`}>
          <div className="quota-pop-title">{quotaTitle(host)}</div>
          {popEntries.map((entry) => (
            <PopRow key={entryReactKey(entry)} entry={entry} host={host} />
          ))}
          {/* 一行就夠：每個帳號各印一次「更新於」時，那幾個時間差不到一分鐘。 */}
          {freshest ? <p className="quota-pop-foot">更新於 {fmtTime(freshest)} · 時間為重置時刻</p> : null}
        </div>
      ) : null}
    </div>
  )
}
