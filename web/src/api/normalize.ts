/**
 * Tolerant decoders for daemon payloads.
 *
 * The backend is written in parallel with this UI, so every reader here accepts the
 * plausible shapes rather than one exact schema:
 *   - `bots[].args` may arrive as `args: string[]` or `args_json: "[...]"`
 *   - booleans may arrive as SQLite integers (`autostart: 0 | 1`)
 *   - `/api/state` may nest bots under projects, or list them flat
 *   - runs may be `runs` / `active_runs`, or embedded on each bot as `run`
 * Everything downstream of this module sees the types in `types.ts`.
 */

import type {
  AgentStatus,
  AppState,
  Bot,
  BotKind,
  Host,
  Identity,
  Lamp,
  Message,
  MessageRole,
  MessageSource,
  MessagesPage,
  Project,
  Run,
  RunState,
  TerminalSnapshot,
  TerminalSource,
  Turn,
  TurnDelivery,
  TurnOrigin,
  TurnStatus,
} from './types'
import { BOT_KINDS } from './types'

type Rec = Record<string, unknown>

const isRec = (v: unknown): v is Rec => typeof v === 'object' && v !== null && !Array.isArray(v)

function str(v: unknown, fallback = ''): string {
  if (typeof v === 'string') return v
  if (typeof v === 'number' || typeof v === 'boolean') return String(v)
  return fallback
}

function optStr(v: unknown): string | null {
  return typeof v === 'string' && v.length > 0 ? v : null
}

function bool(v: unknown, fallback = false): boolean {
  if (typeof v === 'boolean') return v
  if (typeof v === 'number') return v !== 0
  if (typeof v === 'string') return v === 'true' || v === '1'
  return fallback
}

function num(v: unknown, fallback = 0): number {
  if (typeof v === 'number' && Number.isFinite(v)) return v
  if (typeof v === 'string' && v.trim() !== '' && Number.isFinite(Number(v))) return Number(v)
  return fallback
}

/** First present key among `keys`. */
function pick(o: Rec, ...keys: string[]): unknown {
  for (const k of keys) if (o[k] !== undefined && o[k] !== null) return o[k]
  return undefined
}

function arr(v: unknown): unknown[] {
  return Array.isArray(v) ? v : []
}

function oneOf<T extends string>(v: unknown, allowed: readonly T[], fallback: T): T {
  const s = typeof v === 'string' ? v : ''
  return (allowed as readonly string[]).includes(s) ? (s as T) : fallback
}

const RUN_STATES = ['starting', 'running', 'stopping', 'stopped', 'exited'] as const
const AGENT_STATUSES = ['idle', 'working', 'blocked', 'unknown'] as const
const TURN_STATUSES = ['in_flight', 'completed', 'completed_fallback', 'failed'] as const
const DELIVERIES = ['pending', 'ok', 'unknown', 'failed'] as const
const ORIGINS = ['web', 'external'] as const
const ROLES = ['user', 'assistant', 'system'] as const
const SOURCES = ['web', 'hook', 'transcript', 'terminal_fallback', 'system'] as const

/**
 * SPEC §11.6 `hosts[]`. The daemon may report the connection flag as `connected` /
 * `ok` / `up` and the failure text as `error` / `last_error` / `message`.
 */
export function toHost(v: unknown): Host | null {
  if (!isRec(v)) return null
  const name = str(pick(v, 'name', 'host', 'id'))
  if (!name) return null
  return {
    name,
    ssh: str(pick(v, 'ssh', 'target', 'ssh_target')),
    ssh_port: num(pick(v, 'ssh_port', 'port'), 22),
    herdr_session: str(pick(v, 'herdr_session', 'session'), 'agents-manager'),
    remote_path: str(pick(v, 'remote_path', 'path')),
    hook_port: num(pick(v, 'hook_port'), 0),
    connected: bool(pick(v, 'connected', 'ok', 'up'), false),
    error: optStr(pick(v, 'error', 'last_error', 'message', 'reason')),
  }
}

/**
 * `hosts` arrives as an array in `GET /api/state` but as a `{name: {connected, error}}`
 * map in the `daemon_status` frame (SPEC §11.6); accept both.
 */
export function hostArray(v: unknown): unknown[] {
  if (Array.isArray(v)) return v
  if (isRec(v)) return Object.entries(v).map(([name, val]) => (isRec(val) ? { name, ...val } : { name }))
  return []
}

export function toProject(v: unknown): Project | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'project_id'))
  if (!id) return null
  const path = str(pick(v, 'path', 'cwd', 'dir'))
  return {
    id,
    path,
    label: str(pick(v, 'label', 'name'), path.split('/').pop() ?? id),
    workspace_id: optStr(v.workspace_id),
    host: str(pick(v, 'host', 'host_name'), 'local') || 'local',
    created_at: str(v.created_at),
  }
}

export function toBot(v: unknown, projectId?: string): Bot | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'bot_id'))
  if (!id) return null
  let args: string[] = []
  const rawArgs = pick(v, 'args', 'args_json')
  if (Array.isArray(rawArgs)) args = rawArgs.map((a) => str(a))
  else if (typeof rawArgs === 'string') {
    try {
      const parsed: unknown = JSON.parse(rawArgs)
      if (Array.isArray(parsed)) args = parsed.map((a) => str(a))
    } catch {
      args = rawArgs.split(/\s+/).filter(Boolean)
    }
  }
  return {
    id,
    project_id: str(pick(v, 'project_id', 'projectId'), projectId ?? ''),
    name: str(pick(v, 'name', 'label'), id),
    kind: oneOf<BotKind>(v.kind, BOT_KINDS, 'claude'),
    // API.md v3.3; older daemons omit it entirely → treated as "no explicit model".
    model: optStr(pick(v, 'model')),
    args,
    autostart: bool(v.autostart),
    inject_hooks: bool(v.inject_hooks, true),
    auto_approve: bool(v.auto_approve, true),
    identity: (() => {
      const i = pick(v, 'identity')
      return typeof i === 'string' && i.trim() ? i : null
    })(),
    env: envMap(pick(v, 'env', 'env_json')),
    created_at: str(v.created_at),
  }
}

/** `{K: V}` or a JSON string of one; anything else → `{}`. */
export function envMap(v: unknown): Record<string, string> {
  let raw: unknown = v
  if (typeof raw === 'string') {
    try {
      raw = JSON.parse(raw)
    } catch {
      return {}
    }
  }
  if (!isRec(raw)) return {}
  const out: Record<string, string> = {}
  for (const [k, val] of Object.entries(raw)) if (k) out[k] = str(val)
  return out
}

export function toIdentity(v: unknown): Identity | null {
  if (!isRec(v)) return null
  const name = str(pick(v, 'name'))
  if (!name) return null
  const rawArgs = pick(v, 'args')
  return {
    name,
    kind: oneOf<BotKind>(v.kind, BOT_KINDS, 'claude'),
    env: envMap(pick(v, 'env')),
    args: Array.isArray(rawArgs) ? rawArgs.map((a) => str(a)) : [],
  }
}

export function toRun(v: unknown, botId?: string): Run | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'run_id'))
  const bot_id = str(pick(v, 'bot_id', 'botId'), botId ?? '')
  if (!id || !bot_id) return null
  return {
    id,
    bot_id,
    state: oneOf<RunState>(pick(v, 'state', 'run_state'), RUN_STATES, 'running'),
    agent_status: oneOf<AgentStatus>(pick(v, 'agent_status', 'agentStatus', 'status'), AGENT_STATUSES, 'unknown'),
    workspace_id: optStr(v.workspace_id),
    pane_id: optStr(v.pane_id),
    adopted: bool(v.adopted),
    native_session_id: optStr(v.native_session_id),
    transcript_path: optStr(v.transcript_path),
    started_at: str(v.started_at),
    ended_at: optStr(v.ended_at),
  }
}

export function toTurn(v: unknown, botId?: string): Turn | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'turn_id'))
  if (!id) return null
  return {
    id,
    conversation_id: str(v.conversation_id),
    run_id: optStr(v.run_id),
    bot_id: optStr(v.bot_id) ?? botId ?? null,
    origin: oneOf<TurnOrigin>(v.origin, ORIGINS, 'web'),
    status: oneOf<TurnStatus>(v.status, TURN_STATUSES, 'in_flight'),
    delivery: oneOf<TurnDelivery>(v.delivery, DELIVERIES, 'pending'),
    client_request_id: optStr(v.client_request_id),
    created_at: str(v.created_at),
    completed_at: optStr(v.completed_at),
  }
}

export function toMessage(v: unknown, botId?: string): Message | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'message_id'))
  if (!id) return null
  return {
    id,
    conversation_id: str(v.conversation_id),
    turn_id: optStr(v.turn_id),
    bot_id: optStr(v.bot_id) ?? botId ?? null,
    role: oneOf<MessageRole>(v.role, ROLES, 'system'),
    content: str(pick(v, 'content', 'text', 'body')),
    source: oneOf<MessageSource>(v.source, SOURCES, 'system'),
    incomplete: bool(v.incomplete),
    created_at: str(v.created_at),
  }
}

/** Sort ascending by created_at, falling back to id (ULIDs sort lexicographically by time). */
export function sortByTime<T extends { created_at: string; id: string }>(items: T[]): T[] {
  return [...items].sort((a, b) => {
    const t = a.created_at.localeCompare(b.created_at)
    return t !== 0 ? t : a.id.localeCompare(b.id)
  })
}

export function toState(raw: unknown): AppState {
  const root = isRec(raw) ? raw : {}
  const hosts: Host[] = []
  const identities: Identity[] = []
  const projects: Project[] = []
  const bots: Bot[] = []
  const runs: Run[] = []
  const turns: Turn[] = []

  for (const h of hostArray(pick(root, 'hosts', 'host_list'))) {
    // API.md: `hosts[0]` is always the reserved `local` entry (ssh fields null). The UI
    // models the local machine separately (`state.connected`), so drop it here.
    const host = toHost(h)
    if (host && host.name !== 'local' && !hosts.some((x) => x.name === host.name)) hosts.push(host)
  }

  for (const i of arr(pick(root, 'identities', 'identity_list'))) {
    const ident = toIdentity(i)
    if (ident && !identities.some((x) => x.name === ident.name)) identities.push(ident)
  }

  for (const p of arr(pick(root, 'projects', 'project_list'))) {
    const project = toProject(p)
    if (!project) continue
    projects.push(project)
    if (isRec(p)) {
      for (const b of arr(pick(p, 'bots', 'bot_list'))) collectBot(b, project.id)
    }
  }
  for (const b of arr(pick(root, 'bots', 'bot_list'))) collectBot(b)

  for (const r of arr(pick(root, 'runs', 'active_runs', 'activeRuns'))) {
    const run = toRun(r)
    if (run) runs.push(run)
  }
  for (const t of arr(pick(root, 'turns', 'in_flight_turns'))) {
    const turn = toTurn(t)
    if (turn) turns.push(turn)
  }

  function collectBot(b: unknown, projectId?: string) {
    const bot = toBot(b, projectId)
    if (!bot || bots.some((x) => x.id === bot.id)) return
    bots.push(bot)
    if (isRec(b)) {
      const embedded = pick(b, 'run', 'active_run')
      const run = toRun(embedded, bot.id)
      if (run && !runs.some((x) => x.id === run.id)) runs.push(run)
      const t = toTurn(pick(b, 'in_flight_turn', 'turn'), bot.id)
      if (t && !turns.some((x) => x.id === t.id)) turns.push(t)
    }
  }

  return {
    daemon_seq: num(pick(root, 'daemon_seq', 'seq'), 0),
    connected: bool(pick(root, 'connected', 'herdr_connected'), true),
    hosts,
    identities,
    projects,
    bots,
    runs,
    turns,
  }
}

export function toMessages(raw: unknown, botId: string): Message[] {
  const list = Array.isArray(raw) ? raw : isRec(raw) ? arr(pick(raw, 'messages', 'items', 'data')) : []
  const out: Message[] = []
  for (const m of list) {
    const msg = toMessage(m, botId)
    if (msg) out.push(msg)
  }
  // The endpoint paginates in descending order (SPEC §7.2); the UI renders ascending.
  return sortByTime(out)
}

export function toMessagesPage(raw: unknown, botId: string): MessagesPage {
  const o = isRec(raw) ? raw : {}
  const turns: Turn[] = []
  for (const t of arr(pick(o, 'turns'))) {
    const turn = toTurn(t, botId)
    if (turn) turns.push(turn)
  }
  return {
    bot_id: str(pick(o, 'bot_id'), botId),
    conversation_id: str(pick(o, 'conversation_id')),
    messages: toMessages(raw, botId),
    turns,
    has_more: bool(pick(o, 'has_more')),
  }
}

/** SPEC §2.2 composite lamp, recomputed from the active run so WS updates stay authoritative. */
export function lampOf(run: Run | undefined | null, connected: boolean): Lamp {
  if (!connected) return 'disconnected'
  if (!run) return 'offline'
  switch (run.state) {
    case 'starting':
      return 'starting'
    case 'stopping':
      return 'stopping'
    case 'stopped':
    case 'exited':
      return 'offline'
    default:
      return run.agent_status === 'idle'
        ? 'idle'
        : run.agent_status === 'working'
          ? 'working'
          : run.agent_status === 'blocked'
            ? 'blocked'
            : 'unknown'
  }
}

export function toTerminal(raw: unknown, source: TerminalSource): TerminalSnapshot {
  if (typeof raw === 'string') return { text: raw, revision: null, truncated: false, source }
  const o = isRec(raw) ? raw : {}
  const textRaw = pick(o, 'text', 'content', 'data', 'output', 'snapshot', 'lines')
  const text = Array.isArray(textRaw) ? textRaw.map((l) => str(l)).join('\n') : str(textRaw)
  const rev = pick(o, 'revision', 'rev')
  return {
    text,
    revision: rev === undefined ? null : num(rev, 0),
    truncated: bool(o.truncated),
    source: oneOf<TerminalSource>(pick(o, 'source'), ['visible', 'recent_unwrapped'], source),
  }
}

/** Best-effort bot_id for a WS payload that may carry it directly or on a nested entity. */
export function frameBotId(data: unknown): string | null {
  if (!isRec(data)) return null
  const direct = optStr(pick(data, 'bot_id', 'botId'))
  if (direct) return direct
  for (const key of ['message', 'turn', 'run', 'bot']) {
    const nested = data[key]
    if (isRec(nested)) {
      const id = optStr(pick(nested, 'bot_id', key === 'bot' ? 'id' : '__none'))
      if (id) return id
    }
  }
  return null
}

/** Unwrap `{message: {...}}` / `{turn: {...}}` / `{run: {...}}` or the bare entity. */
export function unwrap(data: unknown, key: string): unknown {
  if (isRec(data) && isRec(data[key])) return data[key]
  return data
}

export { isRec, num, str, bool, optStr, pick, arr }
