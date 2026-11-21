import { rawTransport } from './index'
import { ApiError } from './types'
import { L } from './mockText'
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

const mock = (): DeployStatus => ({
  live_sha: '74b6142e',
  target_sha: '9f3c2a1d0b7e4c5f8a6d2e1f0c9b8a7d6e5f4c3b',
  target_short: '9f3c2a1d',
  behind: 4,
  code_commits: 2,
  code_changed: true,
  commits: [
    { sha: '9f3c2a1', subject: L('docs(spec): 補 §18.2 立即部署', 'docs(spec): add §18.2 deploy now') },
    { sha: '5e8d7c6', subject: 'fix(daemon): keep queued prompts across a restart window' },
    { sha: '2b4a6c8', subject: 'fix(web): contain clipboard and DOM exceptions' },
    { sha: '1a3b5c7', subject: L('docs: 截圖更新', 'docs: refresh screenshots') },
  ],
  commits_truncated: false,
  running: null,
  working: [{ bot_id: 'mock-b1', name: 'agents-manager-xrjw1x' }],
  log_path: '~/.config/agents-manager/supervisor/AGM/daemon-update.log',
  kick_ready: true,
  error: null,
})
let mockStarted = false

export async function fetchDeployStatus(): Promise<DeploySnapshot> {
  if (rawTransport.mock) {
    const m = mock()
    return { status: mockStarted ? { ...m, running: { kind: 'requested', sha: m.target_sha } } : m, offline: false }
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
}

export async function startDeployNow(sha: string): Promise<DeployStarted> {
  if (rawTransport.mock) {
    mockStarted = true
    return { started: true, sha, short: sha.slice(0, 8), log_path: mock().log_path }
  }
  const raw = (await rawTransport.request('POST', '/deploy/now', { sha })) as Record<string, unknown>
  return { started: raw.started === true, sha: str(raw.sha), short: str(raw.short), log_path: str(raw.log_path) }
}
