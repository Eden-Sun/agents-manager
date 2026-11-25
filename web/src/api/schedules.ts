import { rawTransport } from './index'
import { t } from '../lib/i18n'

export type ScheduleMode = 'once' | 'interval' | 'daily' | 'weekly'
export type ScheduleNotify = 'failure' | 'all' | 'none'
export type ScheduleOutcome = 'success' | 'failed' | 'refused' | 'unknown'
export interface ScheduleRunner { bot_name: string; kind: string; model: string | null; effort: string | null; identity: string | null; source: 'run' | 'bot' }
export interface ScheduleAttempt { runner?: ScheduleRunner | null; attempt_no: number; turn_id: string | null; reply_message_id: string | null; outcome: ScheduleOutcome; outcome_summary: string; started_at?: string | null; finished_at?: string | null; duration_ms?: number | null }

export type OccurrenceStatus = 'pending' | 'queued' | 'done' | 'failed' | 'cancelled' | 'unknown'

export interface ScheduleSpecIn {
  mode: ScheduleMode
  at?: string
  every_secs?: number
  time?: string
  weekdays?: number[]
  slots?: { time: string; label?: string;  until?: string }[]
}

export interface ScheduleOccurrence {
  id: string
  schedule_id: string
  source: 'scheduled' | 'manual' | 'dry_run'
  scheduled_for: string
  generation: number
  bot_id: string
  prompt: string
  slot_label?: string | null
  status: OccurrenceStatus
  waiting_reason: string | null
  turn_status?: string | null
  turn_delivery?: string | null
  turn_start_error?: string | null
  turn_awaits_start?: number | null
  merged_count: number
  conversation_id: string | null
  turn_id: string | null
  error: string | null
  outcome?: ScheduleOutcome | null
  outcome_summary?: string | null
  reply_message_id?: string | null
  attempt_no?: number
  next_retry_at?: string | null
  attempts?: ScheduleAttempt[]
  runner?: ScheduleRunner | null
  started_at?: string | null
  duration_ms?: number | null
  running_since?: string | null
  created_at: string
  updated_at: string
}

export interface Schedule {
  id: string
  code?: string
  project_id: string
  bot_id: string
  bot_available?: boolean
  name: string
  prompt: string
  mode: ScheduleMode
  tz: string
  spec: unknown
  next_due: string | null
  enabled: boolean
  generation: number
  notify: ScheduleNotify
  retry_count?: number
  retry_interval_mins?: number
  clear_after: boolean
  from_base?: boolean
  bot_dedicated?: boolean
  created_at: string
  updated_at: string
  open?: ScheduleOccurrence | null
  last?: ScheduleOccurrence | null
}

export interface ScheduleDetail extends Schedule {
  recent: ScheduleOccurrence[]
}

export interface ScheduleCreateInput {
  client_request_id: string
  name: string
  prompt: string
  bot_id: string
  tz: string
  spec: ScheduleSpecIn
  notify: ScheduleNotify
  retry_count?: number
  retry_interval_mins?: number
  clear_after?: boolean
  from_base?: boolean
}

export interface ScheduleEditInput {
  name?: string
  prompt?: string
  bot_id?: string
  tz?: string
  spec?: ScheduleSpecIn
  notify?: ScheduleNotify
  retry_count?: number
  retry_interval_mins?: number
  clear_after?: boolean
  from_base?: boolean
  enabled?: boolean
}

const enc = encodeURIComponent

function asSchedule(raw: unknown): Schedule {
  const r = raw as Partial<Schedule> | null
  if (!r || typeof r !== 'object' || typeof r.id !== 'string' || typeof r.project_id !== 'string') {
    throw new Error(t('排程資料格式不對（缺 id 或 project_id）'))
  }
  return { ...r, open: r.open ?? null, clear_after: r.clear_after === true, from_base: r.from_base === true } as Schedule
}

function asDetail(raw: unknown): ScheduleDetail {
  const base = asSchedule(raw)
  const recent = (raw as { recent?: unknown }).recent
  return { ...base, recent: Array.isArray(recent) ? (recent as ScheduleOccurrence[]) : [] }
}

function notifyChanged(projectId?: string): void {
  try {
    window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: projectId ? { project_id: projectId } : {} }))
  } catch {
  }
}

export async function listSchedules(projectId: string): Promise<Schedule[]> {
  const raw = (await rawTransport.request('GET', `/projects/${enc(projectId)}/schedules`)) as { schedules?: unknown }
  return Array.isArray(raw.schedules) ? raw.schedules.map(asSchedule) : []
}

export async function rebindSchedules(projectId: string, scheduleIds: string[], botId: string): Promise<void> {
  await rawTransport.request('POST', `/projects/${enc(projectId)}/schedules/rebind`, { schedule_ids: scheduleIds, bot_id: botId })
  notifyChanged(projectId)
}

export async function getSchedule(id: string): Promise<ScheduleDetail> {
  return asDetail(await rawTransport.request('GET', `/schedules/${enc(id)}`))
}

export async function createSchedule(projectId: string, body: ScheduleCreateInput): Promise<Schedule> {
  const r = asSchedule(await rawTransport.request('POST', `/projects/${enc(projectId)}/schedules`, body))
  notifyChanged(projectId)
  return r
}

export async function patchSchedule(id: string, body: ScheduleEditInput): Promise<Schedule> {
  const r = asSchedule(await rawTransport.request('PATCH', `/schedules/${enc(id)}`, body))
  notifyChanged()
  return r
}

export async function deleteSchedule(id: string): Promise<void> {
  await rawTransport.request('DELETE', `/schedules/${enc(id)}`)
  notifyChanged()
}

export async function runScheduleNow(id: string, clientRequestId: string, dryRun = false): Promise<{ occurrence: ScheduleOccurrence | null }> {
  return (await rawTransport.request('POST', `/schedules/${enc(id)}/run`, { client_request_id: clientRequestId, ...(dryRun ? { dry_run: true } : {}) })) as {
    occurrence: ScheduleOccurrence | null
  }
}
