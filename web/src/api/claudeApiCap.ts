import { rawTransport } from './index'
import { isRec, num, optStr } from './normalize'

export interface ClaudeApiCap {
  cap_usd: number | null
  since: string | null
  spent_usd: number
  /** 每回合時間上限（秒）；舊 daemon 沒這欄＝null。 */
  turn_timeout_secs: number | null
  /** 每回合 API 呼叫次數上限；舊 daemon 沒這欄＝null。 */
  max_api_calls_per_turn: number | null
  /** 超過回合成本門檻時降一級 effort（微美元，0 或 null 為關閉）。 */
  auto_lower_effort_turn_cost_microusd: number | null
  /** 清除工具結果門檻（token）；0 為關閉。 */
  clear_tool_uses_at_input_tokens: number | null
}

const rec = (v: unknown): Record<string, unknown> => (isRec(v) ? v : {})

export function toClaudeApiCap(raw: unknown): ClaudeApiCap {
  const r = rec(raw)
  return {
    cap_usd: typeof r.cap_usd === 'number' ? r.cap_usd : null,
    since: optStr(r.since) ?? null,
    spent_usd: num(r.spent_usd, 0),
    turn_timeout_secs: typeof r.turn_timeout_secs === 'number' ? r.turn_timeout_secs : null,
    max_api_calls_per_turn: typeof r.max_api_calls_per_turn === 'number' ? r.max_api_calls_per_turn : null,
    auto_lower_effort_turn_cost_microusd: typeof r.auto_lower_effort_turn_cost_microusd === 'number' ? r.auto_lower_effort_turn_cost_microusd : null,
    clear_tool_uses_at_input_tokens: typeof r.clear_tool_uses_at_input_tokens === 'number' ? r.clear_tool_uses_at_input_tokens : null,
  }
}

export async function fetchClaudeApiCap(): Promise<ClaudeApiCap> {
  return toClaudeApiCap(await rawTransport.request('GET', '/claude-api/cap'))
}

export async function putClaudeApiCap(cap_usd: number | null): Promise<ClaudeApiCap> {
  return toClaudeApiCap(await rawTransport.request('PUT', '/claude-api/cap', { cap_usd }))
}

/** 只改每回合上限（不動總花費上限）：時間、呼叫次數與自動降級成本門檻。 */
export async function putClaudeApiTurnLimits(limits: {
  turn_timeout_secs: number
  max_api_calls_per_turn: number
  auto_lower_effort_turn_cost_microusd?: number | null
}): Promise<ClaudeApiCap> {
  return toClaudeApiCap(await rawTransport.request('PUT', '/claude-api/cap', limits))
}
