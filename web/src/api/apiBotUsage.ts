/** claude-api bot 的累計用量（`GET /api/bots/{id}/api-usage`，docs/API.md；金額為整數微美元）。 */
import { rawTransport } from './index'
import { isRec, num, optStr, pick } from './normalize'

export interface ApiBotUsage {
  attempts: { total: number; recorded: number; reserved: number; unknown: number; failed: number }
  cost_microusd: { recorded: number; reserved: number; unknown: number }
  tokens: { input: number; output: number; cache_read: number; cache_creation: number }
  /** cache_read ÷ (input＋cache_read＋cache_creation)；沒有 recorded 用量時 null。 */
  cache_hit_rate: number | null
  last: { speed: string; at: string | null; input_tokens: number; output_tokens: number } | null
}

const rec = (v: unknown): Record<string, unknown> => (isRec(v) ? v : {})
const n = (o: Record<string, unknown>, k: string) => num(pick(o, k), 0)

export function toApiBotUsage(raw: unknown): ApiBotUsage {
  const r = rec(raw)
  const a = rec(r.attempts)
  const c = rec(r.cost_microusd)
  const tk = rec(r.tokens)
  const last = isRec(r.last) ? r.last : null
  const rate = r.cache_hit_rate
  return {
    attempts: { total: n(a, 'total'), recorded: n(a, 'recorded'), reserved: n(a, 'reserved'), unknown: n(a, 'unknown'), failed: n(a, 'failed') },
    cost_microusd: { recorded: n(c, 'recorded'), reserved: n(c, 'reserved'), unknown: n(c, 'unknown') },
    tokens: { input: n(tk, 'input'), output: n(tk, 'output'), cache_read: n(tk, 'cache_read'), cache_creation: n(tk, 'cache_creation') },
    cache_hit_rate: typeof rate === 'number' && Number.isFinite(rate) ? rate : null,
    last: last ? { speed: optStr(pick(last, 'speed')) ?? 'standard', at: optStr(pick(last, 'at')), input_tokens: n(last, 'input_tokens'), output_tokens: n(last, 'output_tokens') } : null,
  }
}

export async function fetchApiBotUsage(botId: string): Promise<ApiBotUsage> {
  return toApiBotUsage(await rawTransport.request('GET', `/bots/${encodeURIComponent(botId)}/api-usage`))
}

/** 累計花費（美元）＝(recorded＋reserved＋unknown)／1e6：預留與結果不明的都先當花掉算，跟驗收停線同一個口徑。 */
export function apiBotSpendUsd(u: ApiBotUsage): number {
  return (u.cost_microusd.recorded + u.cost_microusd.reserved + u.cost_microusd.unknown) / 1e6
}
