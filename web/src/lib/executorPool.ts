/**
 * 執行者池條目的純函式：預設建議、模型視窗上限、重複判斷、產生條目 id。
 * 元件與測試共用；不碰 store／網路。
 */
import { agyModelAllowed, type BotKind, type ExecutorPoolEntry, type ModelInfo } from '../api/types'

export const POOL_MAX_ENTRIES = 20
export const POOL_MAX_COUNT = 10
export const POOL_CONTEXT_MIN = 32000
/** 沒有 `context_window` 欄位時的後備上限（claude 只有清單含 `[1m]` 變體才給 1M）。 */
export const CONTEXT_FALLBACK = { claude: 200000, claude1m: 1000000, codex: 400000, other: 200000 } as const

export interface PoolSuggestion {
  kind: BotKind
  /** 在模型 id／display_name 上比對家族，取版本號最大的一個。 */
  family: RegExp
  /** 建議的強度；模型沒列這個強度（或 agy 的強度寫在 slug 裡）時退成「預設」。 */
  effort: string | null
  context_tokens: number | null
  count: number
}

/** 使用者 2026-10-31 指定的五條建議（不自動加，使用者勾選後才加入）。 */
export const POOL_SUGGESTIONS: readonly PoolSuggestion[] = [
  { kind: 'claude', family: /haiku/i, effort: 'low', context_tokens: 200000, count: 2 },
  { kind: 'claude', family: /sonnet/i, effort: 'medium', context_tokens: 200000, count: 1 },
  { kind: 'codex', family: /terra/i, effort: 'medium', context_tokens: 256000, count: 1 },
  { kind: 'codex', family: /luna/i, effort: 'xhigh', context_tokens: 256000, count: 1 },
  { kind: 'agy', family: /gemini.*flash/i, effort: 'low', context_tokens: null, count: 2 },
]

/** 版本比較用：id 裡的數字串（`gpt-6.1-terra` → [6,1]）；沒有數字的別名（`haiku`）是 []。 */
function versionOf(id: string): number[] {
  return (id.match(/\d+/g) ?? []).map(Number)
}

function compareVersion(a: number[], b: number[]): number {
  const n = Math.max(a.length, b.length)
  for (let i = 0; i < n; i++) {
    const d = (a[i] ?? -1) - (b[i] ?? -1)
    if (d !== 0) return d
  }
  return 0
}

/** 模型的視窗上限：優先用 daemon 給的 `context_window`，沒有才用後備值。 */
export function contextCap(kind: BotKind, model: ModelInfo, list: readonly ModelInfo[]): number {
  if (typeof model.context_window === 'number' && model.context_window > 0) return model.context_window
  if (kind === 'claude') return list.some((m) => m.id === `${model.id}[1m]`) ? CONTEXT_FALLBACK.claude1m : CONTEXT_FALLBACK.claude
  if (kind === 'codex') return CONTEXT_FALLBACK.codex
  return CONTEXT_FALLBACK.other
}

/**
 * 依家族挑「最新版」：先過濾掉 `[1m]` 變體與（agy 的）非 Gemini，再取版本號最大者；
 * agy 的強度寫在 slug 裡，所以優先挑 `-<effort>` 結尾的那一檔。
 */
export function pickSuggestedModel(s: PoolSuggestion, list: readonly ModelInfo[]): ModelInfo | null {
  let cands = list.filter((m) => !m.id.includes('[1m]') && (s.family.test(m.id) || s.family.test(m.display_name)))
  if (s.kind === 'agy') {
    cands = cands.filter((m) => agyModelAllowed(m.id))
    const withEffort = s.effort ? cands.filter((m) => m.id.endsWith(`-${s.effort}`)) : []
    if (withEffort.length > 0) cands = withEffort
  }
  let best: ModelInfo | null = null
  for (const m of cands) {
    if (!best || compareVersion(versionOf(m.id), versionOf(best.id)) > 0) best = m
  }
  return best
}

/** 建議條目實際要寫入的內容（model 解析不到就回 null，該建議不列）。 */
export function resolveSuggestion(s: PoolSuggestion, list: readonly ModelInfo[]): Omit<ExecutorPoolEntry, 'id'> | null {
  const m = pickSuggestedModel(s, list)
  if (!m) return null
  const effort = s.effort && m.efforts.includes(s.effort) ? s.effort : null
  const cap = contextCap(s.kind, m, list)
  // agy 不支援 context；其他種類的建議上限不得超過模型視窗。
  const context = s.kind === 'agy' ? null : s.context_tokens === null ? null : Math.min(s.context_tokens, cap)
  return { kind: s.kind, model: m.id, effort, context_tokens: context, count: s.count }
}

/** 四元組相同即重複（daemon 回 400 duplicate_pool_entry）。 */
export function poolKey(e: Pick<ExecutorPoolEntry, 'kind' | 'model' | 'effort' | 'context_tokens'>): string {
  return [e.kind, e.model, e.effort ?? '', e.context_tokens ?? ''].join('|')
}

export function isDuplicate(entries: readonly ExecutorPoolEntry[], cand: Omit<ExecutorPoolEntry, 'id'>): boolean {
  const key = poolKey(cand)
  return entries.some((e) => poolKey(e) === key)
}

/** 產生同專案內唯一的短 slug，例 `codex-gpt-6-1-terra-medium-256k`。 */
export function makePoolId(e: Omit<ExecutorPoolEntry, 'id'>, taken: ReadonlySet<string>): string {
  const ctx = e.context_tokens === null ? '' : formatContext(e.context_tokens).toLowerCase()
  // model 多半已帶 kind 前綴（`claude-haiku-5-5`），不重複寫。
  const model = e.model.startsWith(`${e.kind}-`) ? e.model.slice(e.kind.length + 1) : e.model
  const base =
    [e.kind, model, e.effort ?? '', ctx]
      .join('-')
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '')
      .slice(0, 56) || 'entry'
  let id = base
  for (let i = 2; taken.has(id); i++) id = `${base}-${i}`
  return id
}

/** `null`（模型預設）由呼叫端自己翻譯；這裡只排版數字。 */
export function formatContext(tokens: number): string {
  if (tokens % 1000000 === 0) return `${tokens / 1000000}M`
  return `${Math.round(tokens / 1000)}k`
}
