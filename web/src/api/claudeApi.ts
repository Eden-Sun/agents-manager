import { t } from '../lib/i18n'
import { L } from './mockText'
import { rawTransport } from './index'
import { ApiError } from './types'
import { judgeApiMissing } from './judge'

export interface ClaudeApiUse {
  id: string
  label: string
  enabled: boolean
  model: string
  effort: string
  max_per_hour: number
  default_model: string
  default_effort: string
  default_max_per_hour: number
  legacy_section: string | null
  auto: boolean | null
}

export interface ClaudeApiGroup {
  id: string
  label: string
  uses: string[]
  enabled: boolean
  model: string
  effort: string
  max_per_hour: number
  default_model: string
  default_effort: string
  default_max_per_hour: number
  auto: boolean | null
  provider?: string
  openai_allowed?: boolean
  retry_model?: string
}

export interface ClaudeApiOpenAi {
  available: boolean
  unavailable_reason: string | null
  models: string[]
  efforts: string[]
}

export interface ClaudeApiSettings {
  enabled: boolean
  key_present: boolean
  key_error: string | null
  models: string[]
  efforts: string[]
  groups: ClaudeApiGroup[]
  uses: ClaudeApiUse[]
  groupsApi: boolean
  openai?: ClaudeApiOpenAi | null
}

export interface ClaudeApiUsePatch {
  enabled?: boolean
  model?: string
  effort?: string
  max_per_hour?: number
  auto?: boolean
  provider?: string
  retry_model?: string
}

export interface ClaudeApiPatch {
  enabled?: boolean
  uses?: Record<string, ClaudeApiUsePatch>
  groups?: Record<string, ClaudeApiUsePatch>
  token?: string
}

export interface ClaudeApiLedgerRow {
  at: string
  use: string
  model: string
  provider?: string
  ms: number
  ok: boolean
  error_kind: string | null
  input_tokens: number
  output_tokens: number
  stop_reason: string | null
}

export interface ClaudeApiTotals {
  calls: number
  ok: number
  errors: number
  input_tokens: number
  output_tokens: number
}

export interface ClaudeApiLedger {
  totals: ClaudeApiTotals
  by_use: Record<string, ClaudeApiTotals>
  rows: ClaudeApiLedgerRow[]
}

const MODELS = ['claude-haiku-5-5', 'claude-sonnet-5-5', 'claude-opus-5-5']
const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max']
const mockUses = (): ClaudeApiUse[] => [
  { id: 'changelog_review', label: L('影響分析 changelog', 'AGM changelog analysis'), enabled: false, model: 'claude-opus-5-5', effort: 'low', max_per_hour: 30, default_model: 'claude-opus-5-5', default_effort: 'low', default_max_per_hour: 30, legacy_section: null, auto: null },
  { id: 'release_triage', label: L('上游新版分診', 'Upstream release triage'), enabled: false, model: 'claude-opus-5-5', effort: 'low', max_per_hour: 30, default_model: 'claude-opus-5-5', default_effort: 'low', default_max_per_hour: 30, legacy_section: null, auto: null },
  { id: 'changelog_translate', label: L('更新框翻譯', 'Update box translation'), enabled: false, model: 'claude-haiku-5-5', effort: 'low', max_per_hour: 120, default_model: 'claude-haiku-5-5', default_effort: 'low', default_max_per_hour: 120, legacy_section: 'changelog_translate', auto: null },
]
const mockGroups = (): ClaudeApiGroup[] => [
  { id: 'changelog_analysis', label: L('changelog 影響分析', 'Changelog analysis'), uses: ['changelog_review', 'release_triage'], enabled: false, model: 'claude-opus-5-5', effort: 'low', max_per_hour: 30, default_model: 'claude-opus-5-5', default_effort: 'low', default_max_per_hour: 30, auto: null },
  { id: 'translate', label: L('翻譯', 'Translation'), uses: ['message_translate', 'changelog_translate'], enabled: false, model: 'claude-haiku-5-5', effort: 'low', max_per_hour: 120, default_model: 'claude-haiku-5-5', default_effort: 'low', default_max_per_hour: 120, auto: true },
]
let mockStateValue: ClaudeApiSettings | null = null
const initialMockState = (): ClaudeApiSettings => ({ enabled: false, key_present: false, key_error: 'key file unreadable', models: MODELS, efforts: EFFORTS, groups: mockGroups(), uses: mockUses(), groupsApi: true })
const mockStateOf = (): ClaudeApiSettings => (mockStateValue ??= initialMockState())
export function mockTranslateOn(auto = true): void {
  const st = mockStateOf()
  const msg: ClaudeApiUse = { id: 'message_translate', label: L('對話訊息翻譯', 'Message translation'), enabled: true, model: 'claude-haiku-5-5', effort: 'low', max_per_hour: 120, default_model: 'claude-haiku-5-5', default_effort: 'low', default_max_per_hour: 120, legacy_section: null, auto }
  mockStateValue = {
    ...st,
    enabled: true,
    key_present: true,
    key_error: null,
    groups: st.groups.map((g) => (g.id === 'translate' ? { ...g, enabled: true, auto } : g)),
    uses: [...st.uses.filter((u) => u.id !== 'message_translate'), msg],
  }
}
const mockLedger: ClaudeApiLedger = {
  totals: { calls: 4, ok: 3, errors: 1, input_tokens: 9540, output_tokens: 1610 },
  by_use: { changelog_review: { calls: 3, ok: 2, errors: 1, input_tokens: 9120, output_tokens: 1480 }, message_translate: { calls: 1, ok: 1, errors: 0, input_tokens: 420, output_tokens: 130 } },
  rows: [
    { at: '2026-10-23T08:12:03.120Z', use: 'changelog_review', model: 'claude-sonnet-5-5', ms: 4210, ok: true, error_kind: null, input_tokens: 3480, output_tokens: 610, stop_reason: 'end_turn' },
    { at: '2026-10-23T08:10:41.002Z', use: 'changelog_review', model: 'claude-sonnet-5-5', ms: 1200, ok: false, error_kind: 'rate_limited', input_tokens: 0, output_tokens: 0, stop_reason: null },
    { at: '2026-10-23T07:58:10.554Z', use: 'changelog_review', model: 'claude-sonnet-5-5', ms: 5100, ok: true, error_kind: null, input_tokens: 5640, output_tokens: 870, stop_reason: 'end_turn' },
    { at: '2026-10-23T07:50:00.000Z', use: 'message_translate', model: 'gpt-6-luna', provider: 'openai', ms: 900, ok: true, error_kind: null, input_tokens: 420, output_tokens: 130, stop_reason: 'completed' },
  ],
}
const mockOpenAiTotals: ClaudeApiTotals = { calls: 1, ok: 1, errors: 0, input_tokens: 420, output_tokens: 130 }

function parseUse(raw: unknown): ClaudeApiUse | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  if (typeof o.id !== 'string') return null
  const s = (v: unknown, d = '') => (typeof v === 'string' ? v : d)
  const n = (v: unknown, d = 0) => (typeof v === 'number' ? v : d)
  return {
    id: o.id,
    label: s(o.label, o.id),
    enabled: o.enabled === true,
    model: s(o.model),
    effort: s(o.effort),
    max_per_hour: n(o.max_per_hour),
    default_model: s(o.default_model),
    default_effort: s(o.default_effort),
    default_max_per_hour: n(o.default_max_per_hour),
    legacy_section: typeof o.legacy_section === 'string' ? o.legacy_section : null,
    auto: typeof o.auto === 'boolean' ? o.auto : null,
  }
}

function parseGroup(raw: unknown): ClaudeApiGroup | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  if (typeof o.id !== 'string') return null
  const s = (v: unknown, d = '') => (typeof v === 'string' ? v : d)
  const n = (v: unknown, d = 0) => (typeof v === 'number' ? v : d)
  return {
    id: o.id,
    label: s(o.label, o.id),
    uses: Array.isArray(o.uses) ? o.uses.filter((x): x is string => typeof x === 'string') : [],
    enabled: o.enabled === true,
    model: s(o.model),
    effort: s(o.effort),
    max_per_hour: n(o.max_per_hour),
    default_model: s(o.default_model),
    default_effort: s(o.default_effort),
    default_max_per_hour: n(o.default_max_per_hour),
    auto: typeof o.auto === 'boolean' ? o.auto : null,
    provider: o.provider === 'openai' ? 'openai' : 'anthropic',
    openai_allowed: o.openai_allowed === true,
    retry_model: typeof o.retry_model === 'string' ? o.retry_model : '',
  }
}

const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [])

function parse(raw: unknown): ClaudeApiSettings | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  const uses = Array.isArray(o.uses) ? o.uses.map(parseUse).filter((u): u is ClaudeApiUse => u !== null) : []
  const groupsApi = Array.isArray(o.groups)
  const groups = groupsApi
    ? (o.groups as unknown[]).map(parseGroup).filter((g): g is ClaudeApiGroup => g !== null)
    : uses.map((u): ClaudeApiGroup => ({ id: u.id, label: u.label, uses: [u.id], enabled: u.enabled, model: u.model, effort: u.effort, max_per_hour: u.max_per_hour, default_model: u.default_model, default_effort: u.default_effort, default_max_per_hour: u.default_max_per_hour, auto: u.auto }))
  return {
    enabled: o.enabled === true,
    key_present: o.key_present === true,
    key_error: typeof o.key_error === 'string' ? o.key_error : null,
    models: strings(o.models),
    efforts: strings(o.efforts),
    groups,
    uses,
    groupsApi,
    openai: parseOpenAi(o.openai),
  }
}

function parseOpenAi(raw: unknown): ClaudeApiOpenAi | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  return {
    available: o.available === true,
    unavailable_reason: typeof o.unavailable_reason === 'string' ? o.unavailable_reason : null,
    models: strings(o.models),
    efforts: strings(o.efforts),
  }
}

function parseTotals(raw: unknown): ClaudeApiTotals {
  const o = (typeof raw === 'object' && raw !== null ? raw : {}) as Record<string, unknown>
  const n = (v: unknown) => (typeof v === 'number' ? v : 0)
  return { calls: n(o.calls), ok: n(o.ok), errors: n(o.errors), input_tokens: n(o.input_tokens), output_tokens: n(o.output_tokens) }
}

function parseLedger(raw: unknown): ClaudeApiLedger {
  const o = (typeof raw === 'object' && raw !== null ? raw : {}) as Record<string, unknown>
  const by = (typeof o.by_use === 'object' && o.by_use !== null ? o.by_use : {}) as Record<string, unknown>
  const rows = Array.isArray(o.rows) ? o.rows : []
  return {
    totals: parseTotals(o.totals),
    by_use: Object.fromEntries(Object.entries(by).map(([k, v]) => [k, parseTotals(v)])),
    rows: rows.flatMap((r): ClaudeApiLedgerRow[] => {
      if (typeof r !== 'object' || r === null) return []
      const x = r as Record<string, unknown>
      const n = (v: unknown) => (typeof v === 'number' ? v : 0)
      return [
        {
          at: typeof x.at === 'string' ? x.at : '',
          use: typeof x.use === 'string' ? x.use : '',
          model: typeof x.model === 'string' ? x.model : '',
          provider: typeof x.provider === 'string' ? x.provider : undefined,
          ms: n(x.ms),
          ok: x.ok === true,
          error_kind: typeof x.error_kind === 'string' ? x.error_kind : null,
          input_tokens: n(x.input_tokens),
          output_tokens: n(x.output_tokens),
          stop_reason: typeof x.stop_reason === 'string' ? x.stop_reason : null,
        },
      ]
    }),
  }
}

export async function fetchClaudeApiSettings(): Promise<ClaudeApiSettings | null> {
  if (rawTransport.mock) return mockStateOf()
  try {
    return parse(await rawTransport.request('GET', '/claude-api/settings'))
  } catch (e) {
    if (judgeApiMissing(e)) return null
    throw e
  }
}

export type LedgerProvider = 'all' | 'anthropic' | 'openai'

export async function fetchClaudeApiLedger(limit = 10, provider: LedgerProvider = 'all'): Promise<ClaudeApiLedger> {
  if (rawTransport.mock) {
    if (provider === 'all') return mockLedger
    const rows = mockLedger.rows.filter((r) => (r.provider ?? 'anthropic') === provider)
    return { totals: provider === 'openai' ? mockOpenAiTotals : mockLedger.totals, by_use: {}, rows }
  }
  const q = provider === 'all' ? '' : `&provider=${provider}`
  return parseLedger(await rawTransport.request('GET', `/claude-api/ledger?limit=${limit}${q}`))
}

export type ClaudeApiSaveResult = { ok: true; settings: ClaudeApiSettings } | { ok: false; message: string }

export async function saveClaudeApiSettings(patch: ClaudeApiPatch): Promise<ClaudeApiSaveResult> {
  if (rawTransport.mock) {
    const mockState = mockStateOf()
    const hasKey = mockState.key_present || !!patch.token?.trim()
    const groups = mockState.groups.map((g) => ({ ...g, ...(patch.groups?.[g.id] ?? {}) }))
    const uses = mockState.uses.map((u) => {
      const g = groups.find((x) => x.uses.includes(u.id))
      return g ? { ...u, enabled: g.enabled, model: g.model, effort: g.effort, max_per_hour: g.max_per_hour, ...(g.auto !== null && u.auto !== null ? { auto: g.auto } : {}) } : u
    })
    const turningOn = (patch.enabled === true && !mockState.enabled) || groups.some((g, i) => g.enabled && !mockState.groups[i]?.enabled)
    if (turningOn && !hasKey) return { ok: false, message: t('還沒有可用的 API key，先貼上 key 再開。') }
    mockStateValue = { ...mockState, enabled: patch.enabled ?? mockState.enabled, key_present: hasKey, key_error: hasKey ? null : mockState.key_error, groups, uses }
    return { ok: true, settings: mockStateValue }
  }
  try {
    const settings = parse(await rawTransport.request('PUT', '/claude-api/settings', patch))
    return settings ? { ok: true, settings } : { ok: false, message: t('daemon 回了看不懂的內容') }
  } catch (e) {
    if (e instanceof ApiError) return { ok: false, message: e.status === 409 ? t('還沒有可用的 API key，先貼上 key 再開。') : e.message }
    throw e
  }
}
