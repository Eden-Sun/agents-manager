
import { rawTransport } from './index'
import { ApiError } from './types'

export const SUPERVISOR_CANDIDATES = [
  { model: 'fable', effort: 'low', label: 'cc0 fable low' },
  { model: 'opus', effort: 'low', label: 'cc0 opus low' },
] as const

export interface SupervisorInfo {
  configured: boolean
  bot_id: string
  project_id: string
  model: string
  identity: string
  effort: string
  status: string
  pending_count: number
  assignments: SupervisorAssignment[]
  stats: SupervisorRoleStats
  responder: SupervisorResponder
}

export interface SupervisorRoleStats {
  wakes: number
  events_delivered: number
  duplicates: number
  merged: number
  last_wake_at: string | null
  last_wake_reason: string | null
  notify_next_at: string | null
}

export interface SupervisorResponder {
  configured: boolean
  bot_present: boolean
  bot_id: string
  identity: string
  model: string
  effort: string
  status: string
  status_detail: string
  quota_reset_at: string | null
  inbox_open: number
  stats: SupervisorRoleStats
  runtime: { model: string | null; effort: string | null }
}

export interface SupervisorIncident {
  id: string
  kind: string
  resource: string
  severity: string
  status: string
  first_seen_at: string
  occurrences: number
}

export interface SupervisorAssignment {
  id: string
  target_bot_id: string
  turn_id: string
  status: string
  text: string
  created_at: string
  result?: string | null
  turn_status?: string | null
  evidence_complete?: boolean | null
  open: boolean
  awaiting_review: boolean
  legacy_closed: boolean
  review: { decision: string | null; by: string | null; reason: string | null }
}

export type SupervisorAction = 'setup' | 'start' | 'stop' | 'fallback'

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

function s(v: unknown, fallback = ''): string {
  return typeof v === 'string' ? v : fallback
}

function n(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
}

function list(v: unknown): unknown[] {
  return Array.isArray(v) ? v : []
}

const OPEN_STATUSES = ['queued', 'delivered', 'unknown', 'awaiting_review', 'blocked']

export function toAssignment(v: unknown): SupervisorAssignment | null {
  if (!isRec(v)) return null
  const id = s(v.id)
  if (!id) return null
  const status = s(v.status, 'unknown')
  const review = isRec(v.review) ? v.review : {}
  return {
    id,
    target_bot_id: s(v.target_bot_id),
    turn_id: s(v.turn_id),
    status,
    text: s(v.text),
    created_at: s(v.created_at),
    result: typeof v.result === 'string' ? v.result : null,
    turn_status: typeof v.turn_status === 'string' ? v.turn_status : null,
    evidence_complete: typeof v.evidence_complete === 'boolean' ? v.evidence_complete : null,
    open: typeof v.open === 'boolean' ? v.open : OPEN_STATUSES.includes(status),
    awaiting_review: v.awaiting_review === true || status === 'awaiting_review',
    legacy_closed: v.legacy_closed === true,
    review: {
      decision: typeof review.decision === 'string' ? review.decision : null,
      by: typeof review.by === 'string' ? review.by : null,
      reason: typeof review.reason === 'string' ? review.reason : null,
    },
  }
}

export function toIncident(v: unknown): SupervisorIncident | null {
  if (!isRec(v)) return null
  const id = s(v.id)
  if (!id) return null
  return {
    id,
    kind: s(v.kind, 'unknown'),
    resource: s(v.resource),
    severity: s(v.severity, 'degraded'),
    status: s(v.status, 'open'),
    first_seen_at: s(v.first_seen_at),
    occurrences: n(v.occurrences),
  }
}

function toStats(v: unknown): SupervisorRoleStats {
  const o = isRec(v) ? v : {}
  const t = (x: unknown) => (typeof x === 'string' ? x : null)
  return {
    wakes: n(o.wakes),
    events_delivered: n(o.events_delivered),
    duplicates: n(o.duplicates),
    merged: n(o.merged),
    last_wake_at: t(o.last_wake_at),
    last_wake_reason: t(o.last_wake_reason),
    notify_next_at: t(o.notify_next_at),
  }
}

export function toResponder(v: unknown): SupervisorResponder {
  const o = isRec(v) ? v : {}
  return {
    configured: o.configured === true,
    bot_present: typeof o.bot_present === 'boolean' ? o.bot_present : s(o.bot_id) !== '',
    bot_id: s(o.bot_id),
    identity: s(o.identity, 'cc0'),
    model: s(o.model, 'opus'),
    effort: s(o.effort, 'high'),
    status: s(o.status, 'not_configured'),
    status_detail: s(o.status_detail),
    quota_reset_at: typeof o.quota_reset_at === 'string' ? o.quota_reset_at : null,
    inbox_open: n(o.inbox_open),
    stats: toStats(o.stats),
    runtime: {
      model: isRec(o.runtime) && typeof o.runtime.model === 'string' ? o.runtime.model : null,
      effort: isRec(o.runtime) && typeof o.runtime.effort === 'string' ? o.runtime.effort : null,
    },
  }
}

function toInfo(v: unknown): SupervisorInfo {
  const o = isRec(v) ? v : {}
  return {
    configured: o.configured === true,
    bot_id: s(o.bot_id),
    project_id: s(o.project_id),
    identity: s(o.identity, 'cc0'),
    model: s(o.model, SUPERVISOR_CANDIDATES[0].model),
    effort: s(o.effort, 'low'),
    status: s(o.status, 'unknown'),
    pending_count: n(o.pending_count),
    assignments: list(o.assignments).map(toAssignment).filter((a): a is SupervisorAssignment => a !== null),
    stats: toStats(o.stats),
    responder: toResponder(o.responder),
  }
}

const mockState: SupervisorInfo = {
  configured: false,
  bot_id: '',
  project_id: '',
  identity: 'cc0',
  model: SUPERVISOR_CANDIDATES[0].model,
  effort: 'low',
  status: 'not_configured',
  pending_count: 0,
  assignments: [],
  stats: toStats({}),
  responder: toResponder({
    configured: true,
    bot_id: 'bot-agm-responder',
    status: 'idle',
    inbox_open: 1,
    stats: { wakes: 3, events_delivered: 7, duplicates: 12, last_wake_reason: 'bot_request×2, approval_requested' },
  }),
}

function mockAct(action: SupervisorAction): SupervisorInfo {
  if (action === 'setup') {
    if (!mockState.configured) {
      mockState.configured = true
      mockState.bot_id = 'bot-agm'
      mockState.project_id = 'proj-agm'
      mockState.status = 'stopped'
    }
  } else if (action === 'start') {
    mockState.status = 'running'
    mockState.assignments = [
      {
        id: 'asg-1',
        target_bot_id: 'bot-1',
        turn_id: 'turn-1',
        status: 'delivered',
        text: '重現並修正手機登入流程卡在驗證遠端身份的問題',
        created_at: new Date().toISOString(),
        result: null,
        turn_status: null,
        evidence_complete: null,
        open: true,
        awaiting_review: false,
        legacy_closed: false,
        review: { decision: null, by: null, reason: null },
      },
      {
        id: 'asg-2',
        target_bot_id: 'bot-2',
        turn_id: 'turn-2',
        status: 'awaiting_review',
        text: '把 daemon 重建成 release 並回報結果',
        created_at: new Date().toISOString(),
        result: '還在等編譯，好了再回報',
        turn_status: 'completed',
        evidence_complete: true,
        open: true,
        awaiting_review: true,
        legacy_closed: false,
        review: { decision: null, by: null, reason: null },
      },
    ]
    mockState.pending_count = 2
  } else if (action === 'stop') {
    mockState.status = 'stopped'
  } else {
    mockState.model = mockState.model === SUPERVISOR_CANDIDATES[0].model ? SUPERVISOR_CANDIDATES[1].model : SUPERVISOR_CANDIDATES[0].model
  }
  return { ...mockState, assignments: [...mockState.assignments] }
}

export async function fetchSupervisor(): Promise<SupervisorInfo | null> {
  if (rawTransport.mock) return { ...mockState, assignments: [...mockState.assignments] }
  try {
    return toInfo(await rawTransport.request('GET', '/supervisor'))
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null
    throw e
  }
}

export async function supervisorAction(action: SupervisorAction): Promise<SupervisorInfo> {
  if (rawTransport.mock) return mockAct(action)
  return toInfo(await rawTransport.request('POST', `/supervisor/${action}`, {}))
}

export async function fetchIncidents(): Promise<SupervisorIncident[] | null> {
  if (rawTransport.mock) return []
  try {
    const raw = await rawTransport.request('GET', '/supervisor/incidents')
    return list(isRec(raw) ? raw.incidents : null)
      .map(toIncident)
      .filter((i): i is SupervisorIncident => i !== null)
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null
    throw e
  }
}

export interface AssignmentPage {
  assignments: SupervisorAssignment[]
  complete: boolean
}

const ASSIGNMENT_PAGE = 200
const MAX_ASSIGNMENT_PAGES = 50

export async function fetchAssignments(): Promise<AssignmentPage> {
  if (rawTransport.mock) return { assignments: [...mockState.assignments], complete: true }
  const assignments: SupervisorAssignment[] = []
  let before = ''
  for (let page = 0; page < MAX_ASSIGNMENT_PAGES; page++) {
    const q = new URLSearchParams({ limit: String(ASSIGNMENT_PAGE) })
    if (before) q.set('before', before)
    const raw = await rawTransport.request('GET', `/supervisor/assignments?${q.toString()}`)
    const body = isRec(raw) ? raw : {}
    for (const a of list(body.assignments).map(toAssignment)) {
      if (a !== null) assignments.push(a)
    }
    if (body.has_more === false) return { assignments, complete: true }
    if (body.has_more !== true) return { assignments, complete: false }
    before = typeof body.next_cursor === 'string' ? body.next_cursor : ''
    if (!before) return { assignments, complete: false }
  }
  return { assignments, complete: false }
}
