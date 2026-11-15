/** claude-api bot 的累計用量（`GET /api/bots/{id}/api-usage`，docs/API.md；金額為整數微美元）。 */
import { rawTransport } from './index'
import { isRec, num, optStr, pick } from './normalize'

export interface ApiTurnCost {
  turn_id: string
  requests: number
  cost_usd: number
  reserved_usd: number
  in_flight: boolean
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  /** 這一回合送出請求用的速度（`standard`／`fast`）；每回合開始讀一次設定、整回合固定。舊 daemon 沒給＝null。 */
  speed: string | null
}

export interface ApiBotUsage {
  attempts: { total: number; recorded: number; reserved: number; unknown: number; failed: number }
  cost_microusd: { recorded: number; reserved: number; unknown: number }
  tokens: { input: number; output: number; cache_read: number; cache_creation: number }
  /** cache_read ÷ (input＋cache_read＋cache_creation)；沒有 recorded 用量時 null。 */
  cache_hit_rate: number | null
  /** 最近 50 個 turn 各自的花費（美元；cost_usd＝recorded，reserved_usd＝進行中的預留）。 */
  turns: ApiTurnCost[]
  /** prompt cache 倒數依據（daemon 精確給；最近一次已收到回應的請求完成時間＋該回合的 TTL）。 */
  cache: { last_request_at: string | null; ttl_secs: number | null; expires_at: string | null; working: boolean }
  /** 目前 context 大小（最近一次請求最後一個 iteration 的 input＋cache_read＋cache_creation；壓縮後會下降）與壓縮門檻；沒有資料／關閉＝null。 */
  context: { tokens: number | null; compact_at: number | null }
  /** 最近一次 recorded 呼叫：speed 是**上一次呼叫**實際用的（不是目前設定）；三種 input 加起來是它讀到的整段 context。 */
  last: { speed: string; at: string | null; input_tokens: number; output_tokens: number; cache_read_tokens: number; cache_creation_tokens: number } | null
  /** 卡住狀態（checkpoint 未結算、attempt 逾時、啟動閘或 bot 隔離等）；正常時 null。 */
  stuck: { reason: string; message?: string } | null
}

const rec = (v: unknown): Record<string, unknown> => (isRec(v) ? v : {})
const n = (o: Record<string, unknown>, k: string) => num(pick(o, k), 0)

export function toApiBotUsage(raw: unknown): ApiBotUsage {
  const r = rec(raw)
  const a = rec(r.attempts)
  const c = rec(r.cost_microusd)
  const tk = rec(r.tokens)
  const last = isRec(r.last) ? r.last : null
  const stuck = isRec(r.stuck)
    ? {
        reason: optStr(pick(r.stuck as Record<string, unknown>, 'reason')) ?? 'stuck',
        message: optStr(pick(r.stuck as Record<string, unknown>, 'message')) ?? undefined,
      }
    : null
  const rate = r.cache_hit_rate
  const turns = (Array.isArray(r.turns) ? r.turns : []).filter(isRec).map((x): ApiTurnCost => ({
    turn_id: optStr(pick(x, 'turn_id')) ?? '',
    requests: n(x, 'requests'),
    cost_usd: n(x, 'cost_usd'),
    reserved_usd: n(x, 'reserved_usd'),
    in_flight: x.in_flight === true,
    input_tokens: n(x, 'input_tokens'),
    output_tokens: n(x, 'output_tokens'),
    cache_read_tokens: n(x, 'cache_read_tokens'),
    cache_write_tokens: n(x, 'cache_write_tokens'),
    speed: optStr(pick(x, 'speed')),
  }))
  const ctx = rec(r.context)
  const nn = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null)
  return {
    turns,
    cache: { last_request_at: optStr(pick(rec(r.cache), 'last_request_at')), ttl_secs: nn(rec(r.cache).ttl_secs), expires_at: optStr(pick(rec(r.cache), 'expires_at')), working: rec(r.cache).working === true },
    context: { tokens: nn(ctx.tokens), compact_at: nn(ctx.compact_at) },
    attempts: { total: n(a, 'total'), recorded: n(a, 'recorded'), reserved: n(a, 'reserved'), unknown: n(a, 'unknown'), failed: n(a, 'failed') },
    cost_microusd: { recorded: n(c, 'recorded'), reserved: n(c, 'reserved'), unknown: n(c, 'unknown') },
    tokens: { input: n(tk, 'input'), output: n(tk, 'output'), cache_read: n(tk, 'cache_read'), cache_creation: n(tk, 'cache_creation') },
    cache_hit_rate: typeof rate === 'number' && Number.isFinite(rate) ? rate : null,
    last: last ? { speed: optStr(pick(last, 'speed')) ?? 'standard', at: optStr(pick(last, 'at')), input_tokens: n(last, 'input_tokens'), output_tokens: n(last, 'output_tokens'), cache_read_tokens: n(last, 'cache_read_input_tokens'), cache_creation_tokens: n(last, 'cache_creation_input_tokens') } : null,
    stuck,
  }
}

export async function fetchApiBotUsage(botId: string): Promise<ApiBotUsage> {
  return toApiBotUsage(await rawTransport.request('GET', `/bots/${encodeURIComponent(botId)}/api-usage`))
}

/** 5 位數以上用 k：148012 → `148k`。 */
export function formatTokensK(n: number): string {
  return n >= 10_000 ? `${Math.round(n / 1000)}k` : String(n)
}

/** 累計花費（美元）＝(recorded＋reserved＋unknown)／1e6：預留與結果不明的都先當花掉算，跟驗收停線同一個口徑。 */
export function apiBotSpendUsd(u: ApiBotUsage): number {
  return (u.cost_microusd.recorded + u.cost_microusd.reserved + u.cost_microusd.unknown) / 1e6
}

export interface TimelineRequestItem {
  type: 'request'
  ordinal: number
  timestamp: string
  iteration_type: string
  input_tokens: number
  cache_read_tokens: number
  cache_write_tokens: number
  output_tokens: number
  cost_usd: number
  stop_reason: string | null
  duration_ms: number | null
  speed: string | null
}

export interface TimelineToolItem {
  type: 'tool'
  name: string
  duration_ms: number | null
  is_error: boolean
}

export interface TimelineTurnEndItem {
  type: 'turn_end'
  timestamp: string | null
  status: string
  halt_reason: string
}

export type TimelineItem = TimelineRequestItem | TimelineToolItem | TimelineTurnEndItem

export interface CategoryBreakdown {
  tokens: number
  cost_usd: number
  percentage: number
}

export interface ApiTurnBreakdown {
  input: CategoryBreakdown
  cache_read: CategoryBreakdown
  cache_write: CategoryBreakdown
  output: CategoryBreakdown
  compaction: CategoryBreakdown
  server_tool: CategoryBreakdown
}

export interface ApiTurnLargestItem {
  category: string
  label: string
  cost_usd: number
  percentage: number
}

export interface ApiTurnSummary {
  total_cost_usd: number
  breakdown: ApiTurnBreakdown
  largest_item: ApiTurnLargestItem
  suggestions: string[]
}

export interface ApiTurnTimelineData {
  bot_id: string
  turn_id: string
  status: string
  halt_reason: string
  total_cost_usd: number
  duration_ms: number | null
  created_at: string
  completed_at: string | null
  summary: ApiTurnSummary
  timeline: TimelineItem[]
}

export async function fetchApiTurnTimeline(botId: string, turnId: string): Promise<ApiTurnTimelineData> {
  const res = await rawTransport.request('GET', `/bots/${encodeURIComponent(botId)}/turns/${encodeURIComponent(turnId)}/api-timeline`)
  return res as ApiTurnTimelineData
}
