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

/** 舊 daemon 沒有這個端點 → `null`，整個區塊不出現。 */
export async function fetchJudgeSettings(): Promise<JudgeSettings | null> {
  if (rawTransport.mock) return mockState
  try {
    return parse(await rawTransport.request('GET', '/judge/settings'))
  } catch (e) {
    if (e instanceof ApiError) return null
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
