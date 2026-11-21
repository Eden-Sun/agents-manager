import { t } from '../lib/i18n'
import { rawTransport } from './index'
import { ApiError } from './types'

export interface JudgeSettings {
  enabled: boolean
  projects: string[]
  model: string
  key_present: boolean
  key_error: string | null
}

export interface JudgeSettingsPatch {
  enabled?: boolean
  projects?: string[]
  token?: string
}

let mockState: JudgeSettings = { enabled: false, projects: [], model: 'jev-1.13.0', key_present: false, key_error: 'key file unreadable' }

function parse(raw: unknown): JudgeSettings | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  return {
    enabled: o.enabled === true,
    projects: Array.isArray(o.projects) ? o.projects.filter((p): p is string => typeof p === 'string') : [],
    model: typeof o.model === 'string' ? o.model : '',
    key_present: o.key_present === true,
    key_error: typeof o.key_error === 'string' ? o.key_error : null,
  }
}

export function judgeApiMissing(e: unknown): boolean {
  return e instanceof ApiError && (e.status === 404 || e.status === 405)
}

export async function fetchJudgeSettings(): Promise<JudgeSettings | null> {
  if (rawTransport.mock) return mockState
  try {
    return parse(await rawTransport.request('GET', '/judge/settings'))
  } catch (e) {
    if (judgeApiMissing(e)) return null
    throw e
  }
}

export type JudgeSaveResult = { ok: true; settings: JudgeSettings } | { ok: false; message: string }

export async function saveJudgeSettings(patch: JudgeSettingsPatch): Promise<JudgeSaveResult> {
  if (rawTransport.mock) {
    const hasKey = mockState.key_present || !!patch.token?.trim()
    if (patch.enabled && !hasKey) return { ok: false, message: t('還沒有可用的 API key，先貼上 key 再開。') }
    mockState = { ...mockState, ...(patch.enabled === undefined ? {} : { enabled: patch.enabled }), ...(patch.projects ? { projects: patch.projects } : {}), key_present: hasKey, key_error: hasKey ? null : mockState.key_error }
    return { ok: true, settings: mockState }
  }
  try {
    const settings = parse(await rawTransport.request('PUT', '/judge/settings', patch))
    return settings ? { ok: true, settings } : { ok: false, message: t('daemon 回了看不懂的內容') }
  } catch (e) {
    if (e instanceof ApiError) {
      return { ok: false, message: e.status === 409 ? t('還沒有可用的 API key，先貼上 key 再開。') : e.message }
    }
    throw e
  }
}

export interface JudgeShadowRow {
  id: string
  at: string
  bot_id: string | null
  run_id: string | null
  kind: string | null
  matched_line: string | null
  composer_idle: boolean | null
  regex_verdict: string
  jev_is_live_ui: number | null
  jev_same_work: number | null
  model: string | null
  ms: number | null
  input_tokens: number | null
  error: string | null
  cleared_at: string | null
  assignment_id: string | null
  claims_verified: number | null
  asks_parent_action: number | null
}

export interface JudgeShadow {
  enabled: boolean
  projects: string[]
  model: string
  rows: JudgeShadowRow[]
}

const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null)
const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)

function parseShadowRow(raw: unknown): JudgeShadowRow | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  if (typeof o.id !== 'string' || typeof o.regex_verdict !== 'string') return null
  return {
    id: o.id,
    at: str(o.at) ?? '',
    bot_id: str(o.bot_id),
    run_id: str(o.run_id),
    kind: str(o.kind),
    matched_line: str(o.matched_line),
    composer_idle: typeof o.composer_idle === 'boolean' ? o.composer_idle : null,
    regex_verdict: o.regex_verdict,
    jev_is_live_ui: num(o.jev_is_live_ui),
    jev_same_work: num(o.jev_same_work),
    model: str(o.model),
    ms: num(o.ms),
    input_tokens: num(o.input_tokens),
    error: str(o.error),
    cleared_at: str(o.cleared_at),
    assignment_id: str(o.assignment_id),
    claims_verified: num(o.claims_verified),
    asks_parent_action: num(o.asks_parent_action),
  }
}

function parseShadow(raw: unknown): JudgeShadow | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  const rows = Array.isArray(o.rows) ? o.rows.map(parseShadowRow).filter((r): r is JudgeShadowRow => r !== null) : []
  return {
    enabled: o.enabled === true,
    projects: Array.isArray(o.projects) ? o.projects.filter((p): p is string => typeof p === 'string') : [],
    model: typeof o.model === 'string' ? o.model : '',
    rows,
  }
}

export async function fetchJudgeShadow(limit = 200): Promise<JudgeShadow | null> {
  if (rawTransport.mock) return { enabled: false, projects: [], model: 'jev-1.13.0', rows: [] }
  try {
    return parseShadow(await rawTransport.request('GET', `/judge/shadow?limit=${limit}`))
  } catch (e) {
    if (judgeApiMissing(e)) return null
    throw e
  }
}
