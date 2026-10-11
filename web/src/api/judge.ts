/**
 * 環境設定的「Jev 第二意見」（SPEC §4.3c，issue #240）：開關、專案名單、API key。
 *
 * key 只往 daemon 送、永遠不回來：讀到的只有 `key_present` 與不能用的原因。
 */
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
  /** 空字串＝不動現有的 key。 */
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

/**
 * 404／405：這顆 daemon 還沒有 `/judge/settings`（POST 掉進前端 SPA 的 catch-all，那條只收 GET
 * 所以是 405）。**只有這兩個狀態碼**算「舊 daemon」——跟 `api/index.ts` 與 `api/preview.ts`
 * （`previewApiMissing`）同一個約定。
 *
 * 以前這裡是 `if (e instanceof ApiError) return null`，不分狀態碼（issue #466）：500（daemon 自己出錯）、
 * 403（token 不對）、502／504（反向代理）全都被翻譯成「這顆 daemon 還沒有這個功能，需要更新 daemon」，
 * 使用者照著去重建＋重啟 daemon 當然修不好。
 */
export function judgeApiMissing(e: unknown): boolean {
  return e instanceof ApiError && (e.status === 404 || e.status === 405)
}

/** 舊 daemon 沒有這個端點 → `null`，整個區塊不出現。其餘錯誤往外丟，由呼叫端顯示。 */
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
    if (patch.enabled && !hasKey) return { ok: false, message: '還沒有可用的 API key，先貼上 key 再開。' }
    mockState = { ...mockState, ...(patch.enabled === undefined ? {} : { enabled: patch.enabled }), ...(patch.projects ? { projects: patch.projects } : {}), key_present: hasKey, key_error: hasKey ? null : mockState.key_error }
    return { ok: true, settings: mockState }
  }
  try {
    const settings = parse(await rawTransport.request('PUT', '/judge/settings', patch))
    return settings ? { ok: true, settings } : { ok: false, message: 'daemon 回了看不懂的內容' }
  } catch (e) {
    if (e instanceof ApiError) {
      return { ok: false, message: e.status === 409 ? '還沒有可用的 API key，先貼上 key 再開。' : e.message }
    }
    throw e
  }
}

/** 撞限第二意見帳本的一列（`GET /api/judge/shadow`，docs/API.md；`matched_line` 是遮罩後的，key 不回）。 */
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

/**
 * 帳本（SPEC §4.3c）。舊 daemon 沒有這個端點（404／405）→ `null`，面板照常、只把這一區換成說明。
 * 其餘錯誤往外丟，由呼叫端顯示。
 */
export async function fetchJudgeShadow(limit = 200): Promise<JudgeShadow | null> {
  if (rawTransport.mock) return { enabled: false, projects: [], model: 'jev-1.13.0', rows: [] }
  try {
    return parseShadow(await rawTransport.request('GET', `/judge/shadow?limit=${limit}`))
  } catch (e) {
    if (judgeApiMissing(e)) return null
    throw e
  }
}
