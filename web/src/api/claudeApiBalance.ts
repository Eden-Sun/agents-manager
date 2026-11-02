import { rawTransport } from './index'
import { isRec, num, optStr, pick } from './normalize'

export interface ClaudeApiBalanceAnchor {
  usd: number
  at: string
}

export interface ClaudeApiBalance {
  configured: boolean
  anchor?: ClaudeApiBalanceAnchor
  spent_usd?: number
  remaining_usd?: number
  report_through?: string | null
  local_since?: string | null
  as_of?: string | null
  error?: string | null
}

const rec = (v: unknown): Record<string, unknown> => (isRec(v) ? v : {})

export function toClaudeApiBalance(raw: unknown): ClaudeApiBalance {
  const r = rec(raw)
  const configured = Boolean(r.configured)
  if (!configured) {
    return {
      configured: false,
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
