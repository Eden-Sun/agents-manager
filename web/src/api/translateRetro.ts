/**
 * 譯文「複盤」記錄（使用者 2026-11-21）：只給使用者自己看，不送管理者。
 *
 * - `GET /translate-retros?project_id&limit&before`：全部複盤，新到舊；`nextBefore` 原樣帶回就是下一頁。
 * - `GET /messages/:id/translate-retros`：這則訊息的複盤（舊到新）。
 */
import { rawTransport } from './index'
import { ApiError } from './types'

/** 單次模型嘗試（含被升級重翻的那幾次）。 */
export interface RetroAttempt {
  model: string
  effort: string | null
  /** 本地檢查沒過的原因；過了就 null。 */
  reason: string | null
  jevProb: number | null
  costUsd: number | null
}

export interface TranslateRetro {
  at: string
  messageId: string
  botId: string
  projectId: string
  note: string | null
  /** 實際產出這版譯文的模型（與訊息上「(by …)」的名稱同一個來源）。 */
  model: string
  effort: string | null
  promptVersion: string
  costUsd: number | null
  attempts: RetroAttempt[]
  jevProb: number | null
  lowConfidence: boolean
  /** 四項本地檢查各自的結果。 */
  localChecks: { untranslated: boolean; partiallyUntranslated: boolean; markdownStructure: boolean; japaneseOutput: boolean }
  source: string
  translation: string
}

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null)
const bool = (v: unknown): boolean => v === true
const rec = (v: unknown): Record<string, unknown> => (typeof v === 'object' && v !== null ? (v as Record<string, unknown>) : {})

function parseAttempt(raw: unknown): RetroAttempt {
  const o = rec(raw)
  return { model: str(o.model) ?? '', effort: str(o.effort), reason: str(o.reason), jevProb: num(o.jev_prob), costUsd: num(o.cost_usd) }
}

export function parseRetro(raw: unknown): TranslateRetro | null {
  const o = rec(raw)
  const at = str(o.at)
  if (at === null) return null
  const lc = rec(o.local_checks)
  return {
    at,
    messageId: str(o.message_id) ?? '',
    botId: str(o.bot_id) ?? '',
    projectId: str(o.project_id) ?? '',
    note: str(o.note),
    model: str(o.model) ?? '',
    effort: str(o.effort),
    promptVersion: str(o.prompt_version) ?? '',
    costUsd: num(o.cost_usd),
    attempts: Array.isArray(o.attempts) ? o.attempts.map(parseAttempt) : [],
    jevProb: num(o.jev_prob),
    lowConfidence: bool(o.low_confidence),
    localChecks: {
      untranslated: bool(lc.untranslated),
      partiallyUntranslated: bool(lc.partially_untranslated),
      markdownStructure: bool(lc.markdown_structure),
      japaneseOutput: bool(lc.japanese_output),
    },
    source: str(o.source) ?? '',
    translation: str(o.translation) ?? '',
  }
}

/** 列表一頁：`nextBefore` 為 null 就是最後一頁。 */
export async function fetchTranslateRetros(opts: { projectId?: string; limit?: number; before?: string } = {}): Promise<{ retros: TranslateRetro[]; nextBefore: string | null }> {
  if (rawTransport.mock) return { retros: [], nextBefore: null }
  const q = new URLSearchParams()
  if (opts.projectId) q.set('project_id', opts.projectId)
  if (opts.limit) q.set('limit', String(opts.limit))
  if (opts.before) q.set('before', opts.before)
  const qs = q.toString()
  const raw = rec(await rawTransport.request('GET', `/translate-retros${qs ? `?${qs}` : ''}`))
  const retros = Array.isArray(raw.retros) ? raw.retros.map(parseRetro).filter((r): r is TranslateRetro => r !== null) : []
  return { retros, nextBefore: str(raw.next_before) }
}

/** 這則訊息的複盤（舊到新）。舊 daemon 沒有這個端點 → 空陣列，不報錯。 */
export async function fetchMessageTranslateRetros(messageId: string): Promise<TranslateRetro[]> {
  if (rawTransport.mock) return []
  try {
    const raw = rec(await rawTransport.request('GET', `/messages/${encodeURIComponent(messageId)}/translate-retros`))
    return Array.isArray(raw.retros) ? raw.retros.map(parseRetro).filter((r): r is TranslateRetro => r !== null) : []
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405)) return []
    throw e
  }
}
