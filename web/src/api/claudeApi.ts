/**
 * 環境設定的「Claude API」（SPEC §4.3d，issue #1919）：總開關、各用途的開關與模型、API key、用量帳本。
 *
 * key 只往 daemon 送、永遠不回來：讀到的只有 `key_present` 與不能用的原因（比照 Jev，`api/judge.ts`）。
 */
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
  /** 這個用途的設定本來在別的段（`[changelog_translate]`）；不受總開關管。 */
  legacy_section: string | null
}

export interface ClaudeApiSettings {
  enabled: boolean
  key_present: boolean
  key_error: string | null
  models: string[]
  efforts: string[]
  uses: ClaudeApiUse[]
}

export interface ClaudeApiUsePatch {
  enabled?: boolean
  model?: string
  effort?: string
  max_per_hour?: number
}

export interface ClaudeApiPatch {
  enabled?: boolean
  uses?: Record<string, ClaudeApiUsePatch>
  /** 空字串＝不動現有的 key。 */
  token?: string
}

export interface ClaudeApiLedgerRow {
  at: string
  use: string
  model: string
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

const MODELS = ['claude-haiku-5-5', 'claude-sonnet-5-5']
const EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max']
const mockUses = (): ClaudeApiUse[] => [
  { id: 'changelog_review', label: L('AGM 解析 changelog', 'AGM changelog analysis'), enabled: false, model: 'claude-sonnet-5-5', effort: 'medium', max_per_hour: 30, default_model: 'claude-sonnet-5-5', default_effort: 'medium', default_max_per_hour: 30, legacy_section: null }, // i18n-ignore：假資料
  { id: 'changelog_translate', label: L('更新框翻譯', 'Update box translation'), enabled: false, model: 'claude-haiku-5-5', effort: 'low', max_per_hour: 0, default_model: 'claude-haiku-5-5', default_effort: 'low', default_max_per_hour: 0, legacy_section: 'changelog_translate' }, // i18n-ignore：假資料
]
/** 延後建立：示範文字依語言，要等 `?lang=` 套用後才取。 */
let mockStateValue: ClaudeApiSettings | null = null
const initialMockState = (): ClaudeApiSettings => ({ enabled: false, key_present: false, key_error: 'key file unreadable', models: MODELS, efforts: EFFORTS, uses: mockUses() })
const mockStateOf = (): ClaudeApiSettings => (mockStateValue ??= initialMockState())
const mockLedger: ClaudeApiLedger = {
  totals: { calls: 3, ok: 2, errors: 1, input_tokens: 9120, output_tokens: 1480 },
  by_use: { changelog_review: { calls: 3, ok: 2, errors: 1, input_tokens: 9120, output_tokens: 1480 } },
  rows: [
    { at: '2026-10-23T08:12:03.120Z', use: 'changelog_review', model: 'claude-sonnet-5-5', ms: 4210, ok: true, error_kind: null, input_tokens: 3480, output_tokens: 610, stop_reason: 'end_turn' },
    { at: '2026-10-23T08:10:41.002Z', use: 'changelog_review', model: 'claude-sonnet-5-5', ms: 1200, ok: false, error_kind: 'rate_limited', input_tokens: 0, output_tokens: 0, stop_reason: null },
    { at: '2026-10-23T07:58:10.554Z', use: 'changelog_review', model: 'claude-sonnet-5-5', ms: 5100, ok: true, error_kind: null, input_tokens: 5640, output_tokens: 870, stop_reason: 'end_turn' },
  ],
}

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
  }
}

const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [])

function parse(raw: unknown): ClaudeApiSettings | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  return {
    enabled: o.enabled === true,
    key_present: o.key_present === true,
    key_error: typeof o.key_error === 'string' ? o.key_error : null,
    models: strings(o.models),
    efforts: strings(o.efforts),
    uses: Array.isArray(o.uses) ? o.uses.map(parseUse).filter((u): u is ClaudeApiUse => u !== null) : [],
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

/** 舊 daemon 沒有這個端點（404／405）→ `null`，整個區塊顯示「需要更新 daemon」。其餘錯誤往外丟。 */
export async function fetchClaudeApiSettings(): Promise<ClaudeApiSettings | null> {
  if (rawTransport.mock) return mockStateOf()
  try {
    return parse(await rawTransport.request('GET', '/claude-api/settings'))
  } catch (e) {
    if (judgeApiMissing(e)) return null
    throw e
  }
}

export async function fetchClaudeApiLedger(limit = 10): Promise<ClaudeApiLedger> {
  if (rawTransport.mock) return mockLedger
  return parseLedger(await rawTransport.request('GET', `/claude-api/ledger?limit=${limit}`))
}

export type ClaudeApiSaveResult = { ok: true; settings: ClaudeApiSettings } | { ok: false; message: string }

export async function saveClaudeApiSettings(patch: ClaudeApiPatch): Promise<ClaudeApiSaveResult> {
  if (rawTransport.mock) {
    const mockState = mockStateOf()
    const hasKey = mockState.key_present || !!patch.token?.trim()
    const uses = mockState.uses.map((u) => ({ ...u, ...(patch.uses?.[u.id] ?? {}) }))
    const turningOn = (patch.enabled === true && !mockState.enabled) || uses.some((u, i) => u.enabled && !mockState.uses[i]?.enabled)
    if (turningOn && !hasKey) return { ok: false, message: t('還沒有可用的 API key，先貼上 key 再開。') }
    mockStateValue = { ...mockState, enabled: patch.enabled ?? mockState.enabled, key_present: hasKey, key_error: hasKey ? null : mockState.key_error, uses }
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
