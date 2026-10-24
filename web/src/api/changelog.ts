/**
 * `GET /api/changelog`：重啟套用前先看新版改了什麼（2026-09-10 使用者需求）。
 * 傳輸層錯誤也收成 `found:false` + `error`，UI 不會靜默略過。
 */
import { rawTransport } from './index'

export interface ChangelogSection {
  version: string
  body: string
}

export interface ChangelogReply {
  kind: string
  host: string
  installedVersion: string | null
  fromVersion: string | null
  found: boolean
  sections: ChangelogSection[]
  sourceUrl: string
  /** 主要來源之外的備援連結（agy 的 releases 頁）；沒有就是 null。 */
  altUrl: string | null
  error: string | null
}

const CLAUDE_SOURCE = 'https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md'
const CODEX_SOURCE = 'https://github.com/openai/codex/releases'
const HERDR_SOURCE = 'https://github.com/herdrdev/herdr/releases'

// agy、grok 的官方來源，跟後端 `changelog.rs` 的常數一致，且只用這三個主機。
const AGY_SOURCE = 'https://raw.githubusercontent.com/google-antigravity/antigravity-cli/refs/heads/main/CHANGELOG.md'
const AGY_ALT_SOURCE = 'https://github.com/google-antigravity/antigravity-cli/releases'
const GROK_SOURCE_BASE = 'https://storage.googleapis.com/grok-build-public-artifacts/cli/changelogs/'

/** agy、grok 的連結一律用前端自己的：後端還沒認得這兩個 kind 時，回應的 `source_url` 會是 claude 的。 */
export function hasOwnSource(kind: string): boolean {
  return kind === 'agy' || kind === 'grok'
}

/** grok 一版一個檔，要有純數字版本才組得出網址；其餘退回該版以前的目錄。 */
function grokSource(to: string | null): string {
  const v = to?.trim().replace(/^v/, '') ?? ''
  return /^\d+(\.\d+)*$/.test(v) ? `${GROK_SOURCE_BASE}${v}.external.md` : GROK_SOURCE_BASE
}

/** codex 沒有 CHANGELOG.md，只有 releases。 */
function sourceFor(kind: string, to: string | null = null): string {
  if (kind === 'herdr') return HERDR_SOURCE
  if (kind === 'agy') return AGY_SOURCE
  if (kind === 'grok') return grokSource(to)
  return kind === 'codex' ? CODEX_SOURCE : CLAUDE_SOURCE
}

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null
}

function toReply(raw: unknown, kind: string, host: string, to: string | null = null): ChangelogReply {
  const r = isRec(raw) ? raw : {}
  const sections = Array.isArray(r.sections)
    ? r.sections
        .filter(isRec)
        .map((s) => ({ version: String(s.version ?? ''), body: String(s.body ?? '') }))
        .filter((s) => s.version)
    : []
  return {
    kind: typeof r.kind === 'string' ? r.kind : kind,
    host: typeof r.host === 'string' ? r.host : host,
    installedVersion: typeof r.installed_version === 'string' ? r.installed_version : null,
    fromVersion: typeof r.from_version === 'string' ? r.from_version : null,
    found: r.found === true && sections.length > 0,
    sections,
    sourceUrl: hasOwnSource(kind) ? sourceFor(kind, to) : typeof r.source_url === 'string' ? r.source_url : sourceFor(kind),
    altUrl: kind === 'agy' ? AGY_ALT_SOURCE : null,
    error: typeof r.error === 'string' ? r.error : null,
  }
}

export async function fetchChangelog(
  kind: string,
  host: string,
  from: string | null,
  to: string | null = null,
): Promise<ChangelogReply> {
  const q = new URLSearchParams({ kind, host })
  if (from) q.set('from', from)
  // codex 新版還沒進磁碟，目標版本取自 pane 上的 `a -> b`。
  if (to) q.set('to', to)
  try {
    return toReply(await rawTransport.request('GET', `/changelog?${q.toString()}`), kind, host, to)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    return { kind, host, installedVersion: null, fromVersion: from, found: false, sections: [], sourceUrl: sourceFor(kind, to), altUrl: kind === 'agy' ? AGY_ALT_SOURCE : null, error: msg }
  }
}

/** 輪詢只查狀態。超過這個次數就停，畫面上改按「再查一次」（仍是 GET）。 */
export const TRANSLATION_POLL_MS = 1500
export const TRANSLATION_POLL_MAX = 40

/** `tick` 從 1 起算。第 41 次起停止，避免視窗開著就一直打。 */
export function translationPollAction(tick: number): 'poll' | 'stop' {
  return tick > TRANSLATION_POLL_MAX ? 'stop' : 'poll'
}

export interface TranslationUsage {
  inputTokens: number | null
  outputTokens: number | null
  cacheCreationInputTokens: number | null
  cacheReadInputTokens: number | null
}

export interface TranslationAttempt {
  id: string
  state: string
  sent: boolean
  error: string | null
  usage: TranslationUsage | null
}

/** `unavailable` 只有瀏覽器自己用：翻譯這條連不上時，原文照舊，不當成 daemon 的狀態。 */
export type TranslationStatus =
  | 'unconfigured'
  | 'skipped'
  | 'absent'
  | 'reserved'
  | 'in_flight'
  | 'completed'
  | 'failed'
  | 'interrupted'
  | 'unavailable'

export interface TranslationView {
  enabled: boolean
  status: TranslationStatus
  summary: string[]
  sections: ChangelogSection[]
  coverage: 'full' | 'partial' | null
  truncatedInput: boolean
  errorKind: string | null
  error: string | null
  retryAfter: string | null
  model: string | null
  requestedModel: string | null
  lang: string | null
  /** `[changelog_translate] auto`：新版偵測時背景會翻，開框時 absent 也會自動補翻一次（#1948）。 */
  auto: boolean
  attempts: TranslationAttempt[]
}

const TRANSLATION_STATUSES: readonly TranslationStatus[] = [
  'unconfigured', 'skipped', 'absent', 'reserved', 'in_flight', 'completed', 'failed', 'interrupted',
]

function translationQuery(kind: string, host: string, from: string | null, to: string | null): string {
  const q = new URLSearchParams({ kind, host })
  if (from) q.set('from', from)
  if (to) q.set('to', to)
  return q.toString()
}

/** 缺欄或不是有限數字維持 null。明確的 0 是真的讀數，不改成「沒有」。 */
function numOrNull(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}

function toUsage(raw: unknown): TranslationUsage | null {
  if (!isRec(raw)) return null
  return {
    inputTokens: numOrNull(raw.input_tokens),
    outputTokens: numOrNull(raw.output_tokens),
    cacheCreationInputTokens: numOrNull(raw.cache_creation_input_tokens),
    cacheReadInputTokens: numOrNull(raw.cache_read_input_tokens),
  }
}

function toTranslation(raw: unknown): TranslationView {
  const r = isRec(raw) ? raw : {}
  const statusRaw = typeof r.status === 'string' ? r.status : ''
  const status = TRANSLATION_STATUSES.includes(statusRaw as TranslationStatus) ? (statusRaw as TranslationStatus) : 'unavailable'
  const sections = Array.isArray(r.sections)
    ? r.sections.filter(isRec).map((s) => ({ version: String(s.version ?? ''), body: String(s.body ?? '') })).filter((s) => s.version)
    : []
  const summary = Array.isArray(r.summary) ? r.summary.filter((s): s is string => typeof s === 'string' && s.trim() !== '') : []
  const attempts = Array.isArray(r.attempts)
    ? r.attempts.filter(isRec).map((a) => ({
        id: typeof a.id === 'string' ? a.id : '',
        state: typeof a.state === 'string' ? a.state : '',
        sent: a.sent === true,
        error: typeof a.error === 'string' ? a.error : null,
        usage: toUsage(a.usage),
      }))
    : []
  const coverage = r.coverage === 'full' || r.coverage === 'partial' ? r.coverage : null
  return {
    enabled: r.enabled === true,
    status,
    summary,
    sections,
    coverage,
    truncatedInput: r.truncated_input === true,
    errorKind: typeof r.error_kind === 'string' ? r.error_kind : null,
    error: typeof r.error === 'string' ? r.error : null,
    retryAfter: typeof r.retry_after === 'string' ? r.retry_after : null,
    model: typeof r.model === 'string' ? r.model : null,
    requestedModel: typeof r.requested_model === 'string' ? r.requested_model : null,
    lang: typeof r.lang === 'string' ? r.lang : null,
    auto: r.auto === true,
    attempts,
  }
}

function unavailableTranslation(): TranslationView {
  return {
    enabled: false,
    status: 'unavailable',
    summary: [],
    sections: [],
    coverage: null,
    truncatedInput: false,
    errorKind: null,
    error: null,
    retryAfter: null,
    model: null,
    requestedModel: null,
    lang: null,
    auto: false,
    attempts: [],
  }
}

/** 開框時該不該自動送出一次：只有 absent（沒有任何帳本）、auto 開著、這個 key 還沒送過（#1948）。 */
export function shouldAutoTranslate(view: TranslationView | null, alreadySent: boolean): boolean {
  return !!view && view.enabled && view.auto && view.status === 'absent' && !alreadySent
}

/** 退避到期後自動再試的等待毫秒數；不該自動再試時回傳 null。次數用盡（`attempts`）只給使用者按「再試一次」。 */
export function autoRetryDelay(view: TranslationView | null, now: number): number | null {
  if (!view || !view.enabled || !view.auto || view.status !== 'failed' || view.error === 'attempts' || !view.retryAfter) return null
  const at = Date.parse(view.retryAfter)
  if (!Number.isFinite(at)) return null
  return Math.max(AUTO_RETRY_MIN_MS, at - now)
}

/** 地板值：就算daemon回了過去的時間，也不會連續緊密重打。 */
export const AUTO_RETRY_MIN_MS = 5000

/** 只讀。開視窗、輪詢、再查一次都走這支，不會開始翻譯。 */
export async function fetchTranslation(kind: string, host: string, from: string | null, to: string | null): Promise<TranslationView> {
  try {
    return toTranslation(await rawTransport.request('GET', `/changelog/translation?${translationQuery(kind, host, from, to)}`))
  } catch {
    return unavailableTranslation()
  }
}

/** 使用者按翻譯或「再試一次」。 */
export async function postTranslation(kind: string, host: string, from: string | null, to: string | null): Promise<TranslationView> {
  try {
    return toTranslation(await rawTransport.request('POST', `/changelog/translation?${translationQuery(kind, host, from, to)}`, {}))
  } catch {
    return unavailableTranslation()
  }
}
