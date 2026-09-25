/**
 * 「立即部署」的兩支端點（使用者 2026-09-25，SPEC §18.2）：`GET /api/deploy/status` 與 `POST /api/deploy/now`。
 *
 * 讀狀態一律不拋：daemon 回錯（舊 daemon 沒這支）＝`status: null`，按鈕不出現；連不上＝`offline`，
 * 留著上一次的狀態但不給按（跟重建申請 chip 同一套，issue #531）。
 */
import { rawTransport } from './index'
import { ApiError } from './types'
import type { DeployCommit, DeployRunning, DeployStatus } from '../lib/deployNow'

export interface DeploySnapshot {
  status: DeployStatus | null
  offline: boolean
}

const str = (v: unknown): string => (typeof v === 'string' ? v : '')
const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0)

function toStatus(raw: unknown): DeployStatus | null {
  if (typeof raw !== 'object' || raw === null) return null
  const o = raw as Record<string, unknown>
  const commits: DeployCommit[] = Array.isArray(o.commits)
    ? o.commits
        .map((c) => (typeof c === 'object' && c !== null ? (c as Record<string, unknown>) : null))
        .filter((c): c is Record<string, unknown> => c !== null)
        .map((c) => ({ sha: str(c.sha), subject: str(c.subject) }))
    : []
  const r = typeof o.running === 'object' && o.running !== null ? (o.running as Record<string, unknown>) : null
  const running: DeployRunning | null = r
    ? {
        kind: str(r.kind),
        sha: str(r.sha) || undefined,
        resource: str(r.resource) || undefined,
        owner: str(r.owner) || undefined,
        status: str(r.status) || undefined,
        client_request_id: str(r.client_request_id) || undefined,
      }
    : null
  const working = Array.isArray(o.working)
    ? o.working
        .map((w) => (typeof w === 'object' && w !== null ? (w as Record<string, unknown>) : null))
        .filter((w): w is Record<string, unknown> => w !== null)
        .map((w) => ({ bot_id: str(w.bot_id), name: str(w.name) || str(w.bot_id) }))
    : []
  return {
    live_sha: str(o.live_sha),
    target_sha: str(o.target_sha),
    target_short: str(o.target_short) || str(o.target_sha).slice(0, 8),
    behind: num(o.behind),
    code_commits: num(o.code_commits),
    code_changed: o.code_changed === true,
    commits,
    commits_truncated: o.commits_truncated === true,
    running,
    working,
    log_path: str(o.log_path),
    kick_ready: o.kick_ready !== false,
    error: str(o.error) || null,
  }
}

/** mock：落後 4 個、其中 2 個動到程式碼，有一顆 bot 在 working。按下去之後變成「部署中」。 */
const MOCK: DeployStatus = {
  live_sha: '74b6142e',
  target_sha: '9f3c2a1d0b7e4c5f8a6d2e1f0c9b8a7d6e5f4c3b',
  target_short: '9f3c2a1d',
  behind: 4,
  code_commits: 2,
  code_changed: true,
  commits: [
    { sha: '9f3c2a1', subject: 'docs(spec): 補 §18.2 立即部署' },
    { sha: '5e8d7c6', subject: 'fix(daemon): keep queued prompts across a restart window' },
    { sha: '2b4a6c8', subject: 'fix(web): contain clipboard and DOM exceptions' },
    { sha: '1a3b5c7', subject: 'docs: 截圖更新' },
  ],
  commits_truncated: false,
  running: null,
  working: [{ bot_id: 'mock-b1', name: 'agents-manager-xrjw1x' }],
  log_path: '~/.config/agents-manager/supervisor/AGM/daemon-update.log',
  kick_ready: true,
  error: null,
}
let mockStarted = false

export async function fetchDeployStatus(): Promise<DeploySnapshot> {
  if (rawTransport.mock) {
    return { status: mockStarted ? { ...MOCK, running: { kind: 'requested', sha: MOCK.target_sha } } : MOCK, offline: false }
  }
  try {
    return { status: toStatus(await rawTransport.request('GET', '/deploy/status')), offline: false }
  } catch (e) {
    return { status: null, offline: !(e instanceof ApiError) }
  }
}

export interface DeployStarted {
  started: boolean
  sha: string
  short: string
  log_path: string
  approval_id: string
}

/** 部署確認框上的那顆 commit（不是按下去那一刻的 origin/main）。錯誤照拋，交給 `deployNowNotice` 分類。 */
export async function startDeployNow(sha: string): Promise<DeployStarted> {
  if (rawTransport.mock) {
    mockStarted = true
    return { started: true, sha, short: sha.slice(0, 8), log_path: MOCK.log_path, approval_id: 'mock-ap' }
  }
  const raw = (await rawTransport.request('POST', '/deploy/now', { sha })) as Record<string, unknown>
  return { started: raw.started === true, sha: str(raw.sha), short: str(raw.short), log_path: str(raw.log_path), approval_id: str(raw.approval_id) }
}
