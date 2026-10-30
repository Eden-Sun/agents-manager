/**
 * 環境設定的「模型用法模式」（`GET/PUT /api/model-mode`，docs/API.md）：`claude`（預設）或 `claude_codex`。
 *
 * 各模式的說明文字（含選模型策略）由 daemon 給，前端只顯示、不寫死策略。
 */
import { rawTransport } from './index'
import { ApiError } from './types'

export interface ModelModeOption {
  id: string
  label: string
  description: string
}

export interface ModelModeState {
  mode: string
  modes: ModelModeOption[]
}

let mockMode = 'claude'
const MOCK_MODES: ModelModeOption[] = [
  { id: 'claude', label: 'claude', description: 'Default: behavior unchanged.' },
  {
    id: 'claude_codex',
    label: 'claude主codex輔助模式',
    description:
      'Parent: opus. Advisors (decisions, review): fable, gpt-6-astra. Workers: gpt-6.1-terra medium, gpt-6-luna xhigh, haiku, sonnet. Codex children are long-lived per module; claude children are one-shot.',
  },
]

function parse(raw: unknown): ModelModeState | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  if (typeof o.mode !== 'string' || !Array.isArray(o.modes)) return null
  const modes = o.modes.flatMap((m): ModelModeOption[] => {
    if (typeof m !== 'object' || m === null) return []
    const r = m as Record<string, unknown>
    if (typeof r.id !== 'string') return []
    return [{ id: r.id, label: typeof r.label === 'string' ? r.label : r.id, description: typeof r.description === 'string' ? r.description : '' }]
  })
  return { mode: o.mode, modes }
}

/** 404／405＝舊 daemon 沒有這個端點 → `null`，整個區塊不出現。其餘錯誤往外丟。 */
export async function fetchModelMode(): Promise<ModelModeState | null> {
  if (rawTransport.mock) return { mode: mockMode, modes: MOCK_MODES }
  try {
    return parse(await rawTransport.request('GET', '/model-mode'))
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405)) return null
    throw e
  }
}

export type SetModelModeResult = { ok: true; mode: string; needsRestartBots: boolean } | { ok: false; message: string }

export async function setModelMode(mode: string): Promise<SetModelModeResult> {
  if (rawTransport.mock) {
    mockMode = mode
    return { ok: true, mode, needsRestartBots: true }
  }
  try {
    const raw = (await rawTransport.request('PUT', '/model-mode', { mode })) as Record<string, unknown> | null
    return { ok: true, mode: typeof raw?.mode === 'string' ? raw.mode : mode, needsRestartBots: raw?.needs_restart_bots === true }
  } catch (e) {
    if (e instanceof ApiError) return { ok: false, message: e.message }
    throw e
  }
}
