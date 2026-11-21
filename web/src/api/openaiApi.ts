import { t } from '../lib/i18n'
import { rawTransport } from './index'
import { judgeApiMissing } from './judge'

export interface OpenAiApiSettings {
  enabled: boolean
  key_file: string
  key_present: boolean
  key_error: string | null
  total_cap_usd: number | null
  total_cap_since: string | null
  max_turn_cost_microusd: number | null
  spent_usd: number
  spent_bots_usd: number
  spent_uses_usd: number
}

export interface OpenAiApiPatch {
  enabled?: boolean
  key_file?: string
  total_cap_usd?: number | null
  max_turn_cost_microusd?: number | null
}

const rec = (v: unknown): Record<string, unknown> => (typeof v === 'object' && v !== null ? (v as Record<string, unknown>) : {})
const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0)

export function parseOpenAiApiSettings(raw: unknown): OpenAiApiSettings | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = rec(raw)
  const bot = rec(o.api_bot)
  return {
    enabled: o.enabled === true,
    key_file: typeof o.key_file === 'string' ? o.key_file : '',
    key_present: o.key_present === true,
    key_error: typeof o.key_error === 'string' ? o.key_error : null,
    total_cap_usd: typeof bot.total_cap_usd === 'number' ? bot.total_cap_usd : null,
    total_cap_since: typeof bot.total_cap_since === 'string' ? bot.total_cap_since : null,
    max_turn_cost_microusd: typeof bot.max_turn_cost_microusd === 'number' ? bot.max_turn_cost_microusd : null,
    spent_usd: num(o.spent_usd),
    spent_bots_usd: num(o.spent_bots_usd),
    spent_uses_usd: num(o.spent_uses_usd),
  }
}

let mockState: OpenAiApiSettings | null = null
const mockStateOf = (): OpenAiApiSettings =>
  (mockState ??= { enabled: false, key_file: '~/.config/agents-manager/openai-api-key', key_present: false, key_error: 'key file unreadable', total_cap_usd: null, total_cap_since: null, max_turn_cost_microusd: null, spent_usd: 0.0123, spent_bots_usd: 0.01, spent_uses_usd: 0.0023 })

export async function fetchOpenAiApiSettings(): Promise<OpenAiApiSettings | null> {
  if (rawTransport.mock) return mockStateOf()
  try {
    return parseOpenAiApiSettings(await rawTransport.request('GET', '/openai-api/settings'))
  } catch (e) {
    if (judgeApiMissing(e)) return null
    throw e
  }
}

export type OpenAiApiSaveResult = { ok: true; settings: OpenAiApiSettings } | { ok: false; message: string }

export async function saveOpenAiApiSettings(patch: OpenAiApiPatch): Promise<OpenAiApiSaveResult> {
  if (rawTransport.mock) {
    const cur = mockStateOf()
    if (patch.enabled === true && !cur.enabled && !cur.key_present) return { ok: false, message: t('還沒有可用的 OpenAI key（先把 key 放進 key 檔）。') }
    mockState = {
      ...cur,
      enabled: patch.enabled ?? cur.enabled,
      key_file: patch.key_file ?? cur.key_file,
      ...(patch.total_cap_usd === undefined ? {} : { total_cap_usd: patch.total_cap_usd, total_cap_since: patch.total_cap_usd === null ? null : new Date().toISOString() }),
      ...(patch.max_turn_cost_microusd === undefined ? {} : { max_turn_cost_microusd: patch.max_turn_cost_microusd }),
    }
    return { ok: true, settings: mockState }
  }
  try {
    const raw = await rawTransport.request('PUT', '/openai-api/settings', patch)
    const settings = parseOpenAiApiSettings(raw)
    return settings ? { ok: true, settings } : { ok: false, message: t('回應格式不認得') }
  } catch (e) {
    return { ok: false, message: e instanceof Error ? e.message : String(e) }
  }
}
