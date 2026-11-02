import { rawTransport } from './index'
import { isRec, num, optStr } from './normalize'

export interface ClaudeApiCap {
  cap_usd: number | null
  since: string | null
  spent_usd: number
}

const rec = (v: unknown): Record<string, unknown> => (isRec(v) ? v : {})

export function toClaudeApiCap(raw: unknown): ClaudeApiCap {
  const r = rec(raw)
  return {
    cap_usd: typeof r.cap_usd === 'number' ? r.cap_usd : null,
    since: optStr(r.since) ?? null,
    spent_usd: num(r.spent_usd, 0),
  }
}

export async function fetchClaudeApiCap(): Promise<ClaudeApiCap> {
  return toClaudeApiCap(await rawTransport.request('GET', '/claude-api/cap'))
}

export async function putClaudeApiCap(cap_usd: number | null): Promise<ClaudeApiCap> {
  return toClaudeApiCap(await rawTransport.request('PUT', '/claude-api/cap', { cap_usd }))
}
