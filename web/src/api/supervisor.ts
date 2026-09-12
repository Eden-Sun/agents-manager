/**
 * AGM 總管的 API。獨立成一個檔，`index.ts` 只借它 transport。
 *
 * 兩件事這裡要負責：
 *
 * - **舊 daemon 要靜默降級。** 端點還沒部署時回 404，`fetchSupervisor` 回 `null`，
 *   UI 顯示「這個 daemon 還沒有總管功能」而不是一片紅色錯誤。
 * - **mock 模式不能有 live 副作用。** `VITE_MOCK=1` 時整組走本檔內的假狀態機，
 *   一個請求都不會出去；點 setup／start 只改記憶體裡的狀態。
 */

import { rawTransport } from './index'
import { ApiError } from './types'

/** 總管的模型候補順序是**固定**的：先 fable low，不行才 opus low。UI 不提供改。 */
export const SUPERVISOR_CANDIDATES = [
  { model: 'fable', effort: 'low', label: 'cc0 fable low' },
  { model: 'opus', effort: 'low', label: 'cc0 opus low' },
] as const

/** daemon 回的總管狀態。`remote.status` 是遠端入口，跟 `status`（bot 起沒起來）分開看。 */
export interface SupervisorInfo {
  configured: boolean
  bot_id: string
  project_id: string
  model: string
  identity: string
  effort: string
  status: string
  remote: SupervisorRemote
  pending_count: number
  assignments: SupervisorAssignment[]
}

/**
 * 遠端入口（手機）。`status` 是 daemon 算過的：觀測過期或 session 換掉都會退回 `unknown`。
 *
 * 沒有 `active` 這個值。argv 帶了 `--remote-control` 只代表**要求過**（`requested`）；要說它通了
 * 得有帶 actor 的觀測。`capability.status === 'unsupported'` 表示這台 daemon 根本沒有可靠的觀測
 * 來源——那是「不知道」，不是「壞了」。
 */
export interface SupervisorRemote {
  status: string
  url: string
  source: string | null
  observedAt: string | null
  observedBy: string | null
  revoked: string | null
  capability: string
  capabilityReason: string
}

/** 系統層級的故障。跟「AGM 起沒起來」是兩件事，所以分開拿、分開畫。 */
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
  /** 回合自己的結局（completed / completed_fallback / failed / dispatch_failed）。 */
  turn_status?: string | null
  /** false = 回覆是從終端擷取的，可能被截斷；驗收時要自己再查證據。 */
  evidence_complete?: boolean | null
  /** 還沒結案：還在跑、等驗收，或被標成阻塞。 */
  open: boolean
  awaiting_review: boolean
  /** 驗收前就被舊語意關掉的舊資料——關掉了，但沒有人驗收過。 */
  legacy_closed: boolean
  review: { decision: string | null; by: string | null; reason: string | null }
}

export type SupervisorAction = 'setup' | 'start' | 'stop' | 'fallback'

// ------------------------------------------------------------------ 正規化

function isRec(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

function s(v: unknown, fallback = ''): string {
  return typeof v === 'string' ? v : fallback
}

function n(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
}

/** 還沒結案的狀態。回合跑完只到 `awaiting_review`，那不是結案。 */
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
    // daemon 有給就用它的；舊 daemon 沒給就照狀態自己判，不要預設成「已結案」。
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

function toRemote(v: unknown): SupervisorRemote {
  const o = isRec(v) ? v : {}
  const cap = isRec(o.capability) ? o.capability : {}
  return {
    // 舊 daemon 只回 {status,url}；沒有的欄位一律當「不知道」，不要補成好看的值。
    status: s(o.status, 'unknown'),
    url: s(o.url),
    source: typeof o.source === 'string' ? o.source : null,
    observedAt: typeof o.observed_at === 'string' ? o.observed_at : null,
    observedBy: typeof o.observed_by === 'string' ? o.observed_by : null,
    revoked: typeof o.revoked === 'string' ? o.revoked : null,
    capability: s(cap.status, 'unknown'),
    capabilityReason: s(cap.reason),
  }
}

function toInfo(v: unknown): SupervisorInfo {
  const o = isRec(v) ? v : {}
  return {
    configured: o.configured === true,
    bot_id: s(o.bot_id),
    project_id: s(o.project_id),
    // 身分／強度是寫死的，daemon 沒給就用固定值填，別讓畫面空一格。
    identity: s(o.identity, 'cc0'),
    model: s(o.model, SUPERVISOR_CANDIDATES[0].model),
    effort: s(o.effort, 'low'),
    status: s(o.status, 'unknown'),
    remote: toRemote(o.remote),
    pending_count: n(o.pending_count),
    assignments: ((o.assignments as unknown[]) ?? []).map(toAssignment).filter((a): a is SupervisorAssignment => a !== null),
  }
}

// ---------------------------------------------------------------- mock 狀態

/**
 * mock 模式的假總管。刻意從「還沒建立」開始：這是使用者第一次點進來會看到的畫面，
 * 也是最需要被 demo／截圖驗到的那個狀態。
 */
const mockState: SupervisorInfo = {
  configured: false,
  bot_id: '',
  project_id: '',
  identity: 'cc0',
  model: SUPERVISOR_CANDIDATES[0].model,
  effort: 'low',
  status: 'not_configured',
  remote: {
    status: 'unknown',
    url: '',
    source: null,
    observedAt: null,
    observedBy: null,
    revoked: null,
    capability: 'unsupported',
    capabilityReason: '這台 daemon 沒有可靠的 Remote Control 觀測來源',
  },
  pending_count: 0,
  assignments: [],
}

function mockAct(action: SupervisorAction): SupervisorInfo {
  if (action === 'setup') {
    // setup 是冪等的，而且**只建立**——建完仍然是 stopped，要另外按啟動。
    if (!mockState.configured) {
      mockState.configured = true
      mockState.bot_id = 'bot-agm'
      mockState.project_id = 'proj-agm'
      mockState.status = 'stopped'
    }
  } else if (action === 'start') {
    mockState.status = 'running'
    mockState.remote.status = 'requested'
    mockState.remote.source = 'argv'
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
      // 回合跑完、等驗收的那一筆：mock 也要看得到這個狀態，不然它永遠沒被畫過。
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
    mockState.remote = { ...mockState.remote, status: 'unknown', url: '', source: null }
  } else {
    // 候補：切到第二順位。切不動就維持原樣，不假裝成功。
    mockState.model = mockState.model === SUPERVISOR_CANDIDATES[0].model ? SUPERVISOR_CANDIDATES[1].model : SUPERVISOR_CANDIDATES[0].model
  }
  return { ...mockState, remote: { ...mockState.remote }, assignments: [...mockState.assignments] }
}

// -------------------------------------------------------------------- 呼叫

/** 這個 daemon 有沒有總管 API。`null` = 端點不存在（舊 daemon），不是錯誤。 */
export async function fetchSupervisor(): Promise<SupervisorInfo | null> {
  if (rawTransport.mock) return { ...mockState, remote: { ...mockState.remote }, assignments: [...mockState.assignments] }
  try {
    return toInfo(await rawTransport.request('GET', '/supervisor'))
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null
    throw e
  }
}

/** `setup` / `start` / `stop` / `fallback`，各自回新的狀態。 */
export async function supervisorAction(action: SupervisorAction): Promise<SupervisorInfo> {
  if (rawTransport.mock) return mockAct(action)
  return toInfo(await rawTransport.request('POST', `/supervisor/${action}`, {}))
}

/** 未恢復的系統故障。舊 daemon（404）回空陣列，不是錯誤。 */
export async function fetchIncidents(): Promise<SupervisorIncident[]> {
  if (rawTransport.mock) return []
  try {
    const raw = await rawTransport.request('GET', '/supervisor/incidents')
    const list = isRec(raw) ? (raw.incidents as unknown[]) : []
    return (list ?? []).map(toIncident).filter((i): i is SupervisorIncident => i !== null)
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return []
    throw e
  }
}

/** 交辦清單。單獨拉一次是為了在不重整整個狀態的情況下刷新結果。 */
export async function fetchAssignments(): Promise<SupervisorAssignment[]> {
  if (rawTransport.mock) return [...mockState.assignments]
  const raw = await rawTransport.request('GET', '/supervisor/assignments')
  const list = isRec(raw) ? (raw.assignments as unknown[]) : []
  return (list ?? []).map(toAssignment).filter((a): a is SupervisorAssignment => a !== null)
}
