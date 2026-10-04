/**
 * 部署等換版窗口超過 3 分鐘的通知與調度（SPEC §18.10，API.md「部署等太久」，使用者 2026-10-04）。
 * 狀態來源：`GET /api/state` 的 `deploy_wait` 與 WS `deploy_wait`。按鈕：`POST /api/deploy/wait/{escalate,dismiss}`（只收 UI token）。
 */
import { rawTransport } from './index'

export type DeployWaitPhase = 'waiting' | 'swapping' | 'done' | 'abandoned'

export interface DeployWaitBlocker {
  bot_id: string | null
  name: string
  /** `working`／`delivering`／`unreadable`／`lease`。 */
  why: string
}

export interface DeployWait {
  id: string
  commit: string
  since: string
  waited_secs: number
  blockers: DeployWaitBlocker[]
  escalates_at: string | null
  user_escalated: boolean
  dismissed: boolean
  phase: DeployWaitPhase
  rev: number
  summary: string
}

type Rec = Record<string, unknown>
const rec = (v: unknown): Rec => (typeof v === 'object' && v !== null ? (v as Rec) : {})
const str = (v: unknown) => (typeof v === 'string' ? v : '')
const PHASES: DeployWaitPhase[] = ['waiting', 'swapping', 'done', 'abandoned']

/** 看不懂（沒有 id）就當沒有。 */
export function toDeployWait(raw: unknown): DeployWait | null {
  const o = rec(raw)
  const id = str(o.id)
  if (!id) return null
  const phase = PHASES.includes(o.phase as DeployWaitPhase) ? (o.phase as DeployWaitPhase) : 'waiting'
  const blockers = Array.isArray(o.blockers)
    ? o.blockers.map(rec).map((b) => ({ bot_id: str(b.bot_id) || null, name: str(b.name) || '?', why: str(b.why) }))
    : []
  return {
    id,
    commit: str(o.commit),
    since: str(o.since),
    waited_secs: typeof o.waited_secs === 'number' ? o.waited_secs : 0,
    blockers,
    escalates_at: str(o.escalates_at) || null,
    user_escalated: o.user_escalated === true,
    dismissed: o.dismissed === true,
    phase,
    rev: typeof o.rev === 'number' ? o.rev : 0,
    summary: str(o.summary),
  }
}

export async function escalateDeployWait(id: string): Promise<DeployWait | null> {
  return toDeployWait(await rawTransport.request('POST', '/deploy/wait/escalate', { id }))
}

export async function dismissDeployWait(id: string): Promise<DeployWait | null> {
  return toDeployWait(await rawTransport.request('POST', '/deploy/wait/dismiss', { id }))
}

export const BLOCKER_WHY: Record<string, string> = {
  working: '工作中',
  delivering: '送達中',
  unreadable: '讀不到狀態',
  lease: '握著窗口',
}
