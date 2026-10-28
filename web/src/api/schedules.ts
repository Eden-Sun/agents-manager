// 專案排程的 API（daemon `schedules/api.rs`，User-only）。WS `schedules_changed` 只帶 `{project_id, id}`，
// 網頁收到後自己重抓，prompt 不進全域 state。
import { rawTransport } from './index'
import { t } from '../lib/i18n'

export type ScheduleMode = 'once' | 'interval' | 'daily' | 'weekly'
export type ScheduleNotify = 'failure' | 'all' | 'none'
export type OccurrenceStatus = 'pending' | 'queued' | 'done' | 'failed' | 'cancelled' | 'unknown'

export interface ScheduleSpecIn {
  mode: ScheduleMode
  /** once：本地時間 `YYYY-MM-DDTHH:MM` */
  at?: string
  /** interval：秒（最少 60、最多一年） */
  every_secs?: number
  /** daily／weekly：本地 `HH:MM` */
  time?: string
  /** weekly：0=週一 … 6=週日 */
  weekdays?: number[]
}

export interface ScheduleOccurrence {
  id: string
  schedule_id: string
  source: 'scheduled' | 'manual'
  scheduled_for: string
  generation: number
  bot_id: string
  prompt: string
  status: OccurrenceStatus
  /** pending 時的等待原因：bot_busy／stopping／… */
  waiting_reason: string | null
  /** 綁定 turn 的真實狀態與 delivery（UI 分辨等待、執行中、需確認）。 */
  turn_status?: string | null
  turn_delivery?: string | null
  turn_start_error?: string | null
  turn_awaits_start?: number | null
  merged_count: number
  conversation_id: string | null
  turn_id: string | null
  error: string | null
  created_at: string
  updated_at: string
}

export interface Schedule {
  id: string
  project_id: string
  bot_id: string
  name: string
  prompt: string
  mode: ScheduleMode
  tz: string
  spec: unknown
  next_due: string | null
  enabled: boolean
  generation: number
  notify: ScheduleNotify
  /** 跑完就清空對話：每次成功跑完讓綁定的 Bot 開新對話（daemon 只在 done 時清）。 */
  clear_after: boolean
  created_at: string
  updated_at: string
  /** 列表與詳情都帶：未結束的 occurrence（pending／queued），沒有就是 null。 */
  open?: ScheduleOccurrence | null
}

export interface ScheduleDetail extends Schedule {
  recent: ScheduleOccurrence[]
}

export interface ScheduleCreateInput {
  /** 冪等 key：同一次表單送出（含重送）用同一個值。 */
  client_request_id: string
  name: string
  prompt: string
  bot_id: string
  tz: string
  spec: ScheduleSpecIn
  notify: ScheduleNotify
  clear_after?: boolean
}

export interface ScheduleEditInput {
  name?: string
  prompt?: string
  bot_id?: string
  tz?: string
  spec?: ScheduleSpecIn
  notify?: ScheduleNotify
  clear_after?: boolean
  enabled?: boolean
}

const enc = encodeURIComponent

/** 後端回來的資料先驗再用，不 rawcast：缺 id 就是契約壞了，要明確報錯，不能讓元件讀到 undefined。 */
function asSchedule(raw: unknown): Schedule {
  const r = raw as Partial<Schedule> | null
  if (!r || typeof r !== 'object' || typeof r.id !== 'string' || typeof r.project_id !== 'string') {
    throw new Error(t('排程資料格式不對（缺 id 或 project_id）'))
  }
  return { ...r, open: r.open ?? null, clear_after: r.clear_after === true } as Schedule
}

/** 詳情一定帶 `recent`（daemon 的 `get_one`）；缺的話當成空陣列，不讓畫面炸。 */
function asDetail(raw: unknown): ScheduleDetail {
  const base = asSchedule(raw)
  const recent = (raw as { recent?: unknown }).recent
  return { ...base, recent: Array.isArray(recent) ? (recent as ScheduleOccurrence[]) : [] }
}

export async function listSchedules(projectId: string): Promise<Schedule[]> {
  const raw = (await rawTransport.request('GET', `/projects/${enc(projectId)}/schedules`)) as { schedules?: unknown }
  return Array.isArray(raw.schedules) ? raw.schedules.map(asSchedule) : []
}

export async function getSchedule(id: string): Promise<ScheduleDetail> {
  return asDetail(await rawTransport.request('GET', `/schedules/${enc(id)}`))
}

export async function createSchedule(projectId: string, body: ScheduleCreateInput): Promise<Schedule> {
  return asSchedule(await rawTransport.request('POST', `/projects/${enc(projectId)}/schedules`, body))
}

export async function patchSchedule(id: string, body: ScheduleEditInput): Promise<Schedule> {
  return asSchedule(await rawTransport.request('PATCH', `/schedules/${enc(id)}`, body))
}

export async function deleteSchedule(id: string): Promise<void> {
  await rawTransport.request('DELETE', `/schedules/${enc(id)}`)
}

/** 手動執行：只建立並排進既有佇列，不 steer、不 interrupt。 */
export async function runScheduleNow(id: string, clientRequestId: string): Promise<{ occurrence: ScheduleOccurrence | null }> {
  return (await rawTransport.request('POST', `/schedules/${enc(id)}/run`, { client_request_id: clientRequestId })) as {
    occurrence: ScheduleOccurrence | null
  }
}
