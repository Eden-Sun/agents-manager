import { rawTransport } from './index'
import { ApiError } from './types'
import type { RejectDetail } from '../lib/messageTranslationStore'

export interface RetroAttempt {
  model: string
  effort: string | null
  reason: string | null
  jevProb: number | null
  costUsd: number | null
  text: string | null
  textTruncated: boolean
  hasText: boolean
  sameAsFinal: boolean
  checks: TranslateLocalChecks | null
}

export interface TranslateRecordNote {
  at: string
  note: string
}

export interface TranslateLocalChecks {
  untranslated: boolean
  partiallyUntranslated: boolean
  markdownStructure: boolean
  japaneseOutput: boolean
}

export interface TranslateRecord {
  at: string
  messageId: string | null
  botId: string | null
  projectId: string | null
  status: 'done' | 'rejected'
  model: string
  effort: string | null
  promptVersion: string
  costUsd: number | null
  attempts: RetroAttempt[]
  retried: boolean
  jevProb: number | null
  lowConfidence: boolean
  localChecks: TranslateLocalChecks | null
  checkFailed: boolean
  rejected: string | null
  rejectDetail: RejectDetail | null
  source: string
  translation: string
  notes: TranslateRecordNote[]
}

export type TranslateRecordFilter = 'all' | 'retried' | 'failed' | 'rejected'

const str = (v: unknown): string | null => (typeof v === 'string' ? v : null)
const num = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) ? v : null)
const bool = (v: unknown): boolean => v === true
const rec = (v: unknown): Record<string, unknown> => (typeof v === 'object' && v !== null ? (v as Record<string, unknown>) : {})

function parseAttempt(raw: unknown): RetroAttempt {
  const o = rec(raw)
  return {
    model: str(o.model) ?? '',
    effort: str(o.effort),
    reason: str(o.reason),
    jevProb: num(o.jev_prob),
    costUsd: num(o.cost_usd),
    text: str(o.text),
    textTruncated: bool(o.text_truncated),
    hasText: bool(o.has_text) || str(o.text) !== null,
    sameAsFinal: bool(o.same_as_final),
    checks: parseLocalChecks(o.checks),
  }
}

function parseLocalChecks(raw: unknown): TranslateLocalChecks | null {
  if (typeof raw !== 'object' || raw === null) return null
  const lc = raw as Record<string, unknown>
  if (Object.keys(lc).length === 0) return null
  return {
    untranslated: bool(lc.untranslated),
    partiallyUntranslated: bool(lc.partially_untranslated),
    markdownStructure: bool(lc.markdown_structure),
    japaneseOutput: bool(lc.japanese_output),
  }
}

function parseRejectDetail(raw: unknown): RejectDetail | null {
  if (typeof raw !== 'object' || raw === null) return null
  const d = raw as Record<string, unknown>
  if (typeof d.check !== 'string' || typeof d.identical !== 'boolean' || !Array.isArray(d.models) || typeof d.attempts !== 'number') {
    return null
  }
  return {
    check: d.check,
    src_latin: num(d.src_latin) ?? 0,
    src_cjk: num(d.src_cjk) ?? 0,
    reply_latin: num(d.reply_latin) ?? 0,
    reply_cjk: num(d.reply_cjk) ?? 0,
    identical: d.identical,
    models: d.models.filter((m): m is string => typeof m === 'string'),
    attempts: d.attempts,
  }
}

function parseNote(raw: unknown): TranslateRecordNote | null {
  const o = rec(raw)
  const note = str(o.note)
  if (!note) return null
  return { at: str(o.at) ?? '', note }
}

export function parseTranslateRecord(raw: unknown): TranslateRecord | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  const status = o.status === 'rejected' ? 'rejected' : 'done'
  return {
    at: str(o.at) ?? '',
    messageId: str(o.message_id),
    botId: str(o.bot_id),
    projectId: str(o.project_id),
    status,
    model: str(o.model) ?? '',
    effort: str(o.effort),
    promptVersion: str(o.prompt_version) ?? '',
    costUsd: num(o.cost_usd),
    attempts: Array.isArray(o.attempts) ? o.attempts.map(parseAttempt) : [],
    retried: bool(o.retried),
    jevProb: num(o.jev_prob),
    lowConfidence: bool(o.low_confidence),
    localChecks: parseLocalChecks(o.local_checks),
    checkFailed: bool(o.check_failed),
    rejected: str(o.rejected),
    rejectDetail: parseRejectDetail(o.reject_detail),
    source: str(o.source) ?? '',
    translation: str(o.translation) ?? '',
    notes: Array.isArray(o.notes) ? o.notes.map(parseNote).filter((n): n is TranslateRecordNote => n !== null) : [],
  }
}

export async function fetchTranslateRecords(
  opts: { projectId?: string; limit?: number; before?: string; filter?: TranslateRecordFilter } = {},
): Promise<{ records: TranslateRecord[]; nextBefore: string | null }> {
  const q = new URLSearchParams()
  if (opts.projectId) q.set('project_id', opts.projectId)
  if (opts.limit) q.set('limit', String(opts.limit))
  if (opts.before) q.set('before', opts.before)
  if (opts.filter && opts.filter !== 'all') q.set('filter', opts.filter)
  const qs = q.toString()
  const raw = rec(await rawTransport.request('GET', `/translate-records${qs ? `?${qs}` : ''}`))
  const records = Array.isArray(raw.records) ? raw.records.map(parseTranslateRecord).filter((r): r is TranslateRecord => r !== null) : []
  return { records, nextBefore: str(raw.next_before) }
}

export async function fetchMessageTranslateRecord(messageId: string): Promise<TranslateRecord | null> {
  try {
    const raw = rec(await rawTransport.request('GET', `/messages/${encodeURIComponent(messageId)}/translate-record`))
    return parseTranslateRecord(raw.record)
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405)) return null
    throw e
  }
}
