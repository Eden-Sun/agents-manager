import { rawTransport } from './index'
import { isRec, num, optStr } from './normalize'
import { consoleBalanceView, type BalanceView, type ConsoleBalanceStatus, type ConsolePromotion } from './claudeApiBalance'
import { DEFAULT_USER_TIME_ZONE } from '../lib/timeZone'

export interface OpenAiApiBalanceAnchor {
  usd: number
  at: string
}

export interface OpenAiConsoleBalance {
  status: ConsoleBalanceStatus
  total_available_usd?: number
  total_granted_usd?: number
  total_used_usd?: number
  grants: ConsolePromotion[]
  org_title?: string | null
  fetched_at?: string | null
  error?: string | null
}

export interface OpenAiApiBalance {
  enabled: boolean
  configured: boolean
  error?: string
  as_of: string
  anchor?: OpenAiApiBalanceAnchor
  spent_usd?: number
  remaining_usd?: number
  uncertain_usd?: number
  remaining_floor_usd?: number
  console?: OpenAiConsoleBalance
}

const CONSOLE_STATUSES: readonly ConsoleBalanceStatus[] = ['ok', 'logged_out', 'error', 'not_configured']
const usd = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : undefined)

export function toOpenAiConsoleBalance(raw: unknown): OpenAiConsoleBalance | undefined {
  if (!isRec(raw)) return undefined
  return {
    status: CONSOLE_STATUSES.find((s) => s === raw.status) ?? 'error',
    total_available_usd: usd(raw.total_available_usd),
    total_granted_usd: usd(raw.total_granted_usd),
    total_used_usd: usd(raw.total_used_usd),
    grants: Array.isArray(raw.grants)
      ? raw.grants.filter(isRec).map((g) => ({ remaining_usd: num(g.remaining_usd, 0), total_usd: num(g.total_usd, 0), expires_on: optStr(g.expires_on) ?? null }))
      : [],
    org_title: optStr(raw.org_title),
    fetched_at: optStr(raw.fetched_at),
    error: optStr(raw.error),
  }
}

export function toOpenAiApiBalance(raw: unknown): OpenAiApiBalance {
  const r = isRec(raw) ? raw : {}
  const anchor = isRec(r.anchor) ? { usd: num(r.anchor.usd, 0), at: optStr(r.anchor.at) ?? '' } : undefined
  return {
    enabled: r.enabled === true,
    configured: r.configured === true,
    error: optStr(r.error) ?? undefined,
    as_of: optStr(r.as_of) ?? '',
    anchor,
    spent_usd: usd(r.spent_usd),
    remaining_usd: usd(r.remaining_usd),
    uncertain_usd: usd(r.uncertain_usd),
    remaining_floor_usd: usd(r.remaining_floor_usd),
    console: toOpenAiConsoleBalance(r.console),
  }
}

export function openAiBalanceView(b: OpenAiApiBalance | null | undefined, now: Date = new Date(), zone: string = DEFAULT_USER_TIME_ZONE): BalanceView {
  const c = b?.console
  const estimate = b?.configured ? b.remaining_usd : undefined
  return consoleBalanceView(c && { status: c.status, total_usd: c.total_available_usd, promotions: c.grants }, estimate, now, zone)
}

export async function fetchOpenAiApiBalance(): Promise<OpenAiApiBalance> {
  return toOpenAiApiBalance(await rawTransport.request('GET', '/openai-api/balance'))
}

export async function putOpenAiApiBalanceAnchor(usd: number, at?: string): Promise<OpenAiApiBalance> {
  const body = { usd, at }
  return toOpenAiApiBalance(await rawTransport.request('PUT', '/openai-api/balance-anchor', body))
}
