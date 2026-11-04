import { rawTransport } from './index'
import { isRec, num, optStr, pick } from './normalize'

export interface ClaudeApiBalanceAnchor {
  usd: number
  at: string
}

export interface ConsolePromotion {
  remaining_usd: number
  total_usd: number
  expires_on: string | null
}

export type ConsoleBalanceStatus = 'ok' | 'logged_out' | 'parse_failed' | 'error' | 'not_configured'

/** daemon 用 agent-browser 讀 Console Billing 頁拿到的實際餘額（沒有 Anthropic API 可查）。 */
export interface ConsoleBalance {
  status: ConsoleBalanceStatus
  purchased_usd?: number
  promotional_usd?: number
  total_usd?: number
  promotions: ConsolePromotion[]
  fetched_at?: string | null
  error?: string | null
}

export interface ClaudeApiBalance {
  configured: boolean
  console?: ConsoleBalance
  anchor?: ClaudeApiBalanceAnchor
  spent_usd?: number
  remaining_usd?: number
  report_through?: string | null
  local_since?: string | null
  as_of?: string | null
  error?: string | null
}

const rec = (v: unknown): Record<string, unknown> => (isRec(v) ? v : {})

const CONSOLE_STATUSES: readonly ConsoleBalanceStatus[] = ['ok', 'logged_out', 'parse_failed', 'error', 'not_configured']

export function toConsoleBalance(raw: unknown): ConsoleBalance | undefined {
  if (!isRec(raw)) return undefined
  const status = CONSOLE_STATUSES.find((s) => s === raw.status) ?? 'error'
  const promotions = Array.isArray(raw.promotions)
    ? raw.promotions.filter(isRec).map((p) => ({
        remaining_usd: num(p.remaining_usd, 0),
        total_usd: num(p.total_usd, 0),
        expires_on: optStr(p.expires_on) ?? null,
      }))
    : []
  const usd = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : undefined)
  return {
    status,
    purchased_usd: usd(raw.purchased_usd),
    promotional_usd: usd(raw.promotional_usd),
    total_usd: usd(raw.total_usd),
    promotions,
    fetched_at: optStr(raw.fetched_at),
    error: optStr(raw.error),
  }
}

export function toClaudeApiBalance(raw: unknown): ClaudeApiBalance {
  const r = rec(raw)
  const configured = Boolean(r.configured)
  const consoleBalance = toConsoleBalance(r.console)
  if (!configured) {
    return {
      configured: false,
      console: consoleBalance,
      error: optStr(r.error),
    }
  }
  const anchorRaw = rec(r.anchor)
  const anchor: ClaudeApiBalanceAnchor | undefined =
    r.anchor !== undefined
      ? {
          usd: num(pick(anchorRaw, 'usd'), 0),
          at: optStr(pick(anchorRaw, 'at')) ?? '',
        }
      : undefined

  return {
    configured: true,
    console: consoleBalance,
    anchor,
    spent_usd: typeof r.spent_usd === 'number' ? r.spent_usd : undefined,
    remaining_usd: typeof r.remaining_usd === 'number' ? r.remaining_usd : undefined,
    report_through: optStr(r.report_through),
    local_since: optStr(r.local_since),
    as_of: optStr(r.as_of),
    error: optStr(r.error),
  }
}

export async function fetchClaudeApiBalance(): Promise<ClaudeApiBalance> {
  return toClaudeApiBalance(await rawTransport.request('GET', '/claude-api/balance'))
}

export async function putClaudeApiBalanceAnchor(usd: number, at?: string): Promise<ClaudeApiBalance> {
  const body: { usd: number; at?: string } = { usd }
  if (at) body.at = at
  return toClaudeApiBalance(await rawTransport.request('PUT', '/claude-api/balance-anchor', body))
}

/** 最近一筆促銷幾天內到期就警示。 */
export const PROMO_WARN_DAYS = 7

export interface BalanceView {
  /** 要顯示的金額；沒有任何來源可用時是 `undefined`（不畫晶片）。 */
  usd?: number
  /** `console`＝Console 實際值；`estimate`＝錨點推算（含 Console 讀不到而退回）。 */
  source: 'console' | 'estimate'
  /** Console 登入態過期，已退回推算值（有推算值時）。 */
  loggedOut: boolean
  /** 最近到期的促銷（remaining > 0）。 */
  nextExpiry?: ConsolePromotion
  /** 最近一筆促銷 7 天內到期。 */
  expiringSoon: boolean
}

const parseDay = (iso: string): number => {
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(iso)
  return m ? Date.UTC(Number(m[1]), Number(m[2]) - 1, Number(m[3])) : Number.NaN
}

/** 把回應整理成晶片要的資訊：有 console.ok 用實際值，否則退回推算值。`now` 供測試注入。 */
export function balanceView(b: ClaudeApiBalance | null | undefined, now: Date = new Date()): BalanceView {
  const c = b?.console
  const estimate = b?.configured && b.remaining_usd !== undefined ? b.remaining_usd : undefined
  if (c?.status === 'ok' && c.total_usd !== undefined) {
    const upcoming = c.promotions
      .filter((p) => p.remaining_usd > 0 && p.expires_on && !Number.isNaN(parseDay(p.expires_on)))
      .sort((x, y) => parseDay(x.expires_on as string) - parseDay(y.expires_on as string))
    const nextExpiry = upcoming[0]
    const today = Date.UTC(now.getFullYear(), now.getMonth(), now.getDate())
    const days = nextExpiry ? (parseDay(nextExpiry.expires_on as string) - today) / 86_400_000 : Number.POSITIVE_INFINITY
    return { usd: c.total_usd, source: 'console', loggedOut: false, nextExpiry, expiringSoon: days <= PROMO_WARN_DAYS }
  }
  return { usd: estimate, source: 'estimate', loggedOut: c?.status === 'logged_out', expiringSoon: false }
}
