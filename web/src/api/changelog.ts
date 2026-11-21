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
  altUrl: string | null
  error: string | null
}

const CLAUDE_SOURCE = 'https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md'
const CODEX_SOURCE = 'https://github.com/openai/codex/releases'
const HERDR_SOURCE = 'https://github.com/herdrdev/herdr/releases'

const AGY_SOURCE = 'https://raw.githubusercontent.com/google-antigravity/antigravity-cli/refs/heads/main/CHANGELOG.md'
const AGY_ALT_SOURCE = 'https://github.com/google-antigravity/antigravity-cli/releases'
const GROK_SOURCE_BASE = 'https://storage.googleapis.com/grok-build-public-artifacts/cli/changelogs/'

export function hasOwnSource(kind: string): boolean {
  return kind === 'agy' || kind === 'grok'
}

function grokSource(to: string | null): string {
  const v = to?.trim().replace(/^v/, '') ?? ''
  return /^\d+(\.\d+)*$/.test(v) ? `${GROK_SOURCE_BASE}${v}.external.md` : GROK_SOURCE_BASE
}

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
  if (to) q.set('to', to)
  try {
    return toReply(await rawTransport.request('GET', `/changelog?${q.toString()}`), kind, host, to)
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e)
    return { kind, host, installedVersion: null, fromVersion: from, found: false, sections: [], sourceUrl: sourceFor(kind, to), altUrl: kind === 'agy' ? AGY_ALT_SOURCE : null, error: msg }
  }
}

export const TRANSLATION_POLL_MS = 1500
export const TRANSLATION_POLL_MAX = 40

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

export function shouldAutoTranslate(view: TranslationView | null, alreadySent: boolean): boolean {
  return !!view && view.enabled && view.auto && view.status === 'absent' && !alreadySent
}

export function autoRetryDelay(view: TranslationView | null, now: number): number | null {
  if (!view || !view.enabled || !view.auto || view.status !== 'failed' || view.error === 'attempts' || view.errorKind === 'budget' || !view.retryAfter) return null
  const at = Date.parse(view.retryAfter)
  if (!Number.isFinite(at)) return null
  return Math.max(AUTO_RETRY_MIN_MS, at - now)
}

export const AUTO_RETRY_MIN_MS = 5000

export async function fetchTranslation(kind: string, host: string, from: string | null, to: string | null): Promise<TranslationView> {
  try {
    return toTranslation(await rawTransport.request('GET', `/changelog/translation?${translationQuery(kind, host, from, to)}`))
  } catch {
    return unavailableTranslation()
  }
}

export async function postTranslation(kind: string, host: string, from: string | null, to: string | null): Promise<TranslationView> {
  try {
    return toTranslation(await rawTransport.request('POST', `/changelog/translation?${translationQuery(kind, host, from, to)}`, {}))
  } catch {
    return unavailableTranslation()
  }
}
