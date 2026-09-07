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
  MemOwner,
  MemProcess,
  MemProcesses,
  MemSnapshot,
  AgentStatus,
  AppState,
  Attachment,
  Bot,
  BotKind,
  BotManagedBy,
  BotTeamRef,
  Team,
  TeamBudget,
  TeamDeliver,
  TeamDetail,
  TeamEvent,
  TeamEventKind,
  TeamEventStatus,
  TeamMember,
  TeamPhase,
  TeamRole,
  TeamIssue,
  TeamIssuesSummary,
  TeamIssueState,
  TeamTask,
  TeamTaskState,
  TeamTasksSummary,
  TeamUsage,
  GroupMessage,
  GroupMessagesPage,
  HostShell,
  Host,
  Identity,
  IdentityStatusMap,
  InstallToolResult,
  Issue,
  IssueDetail,
  IssueLabel,
  KindQuota,
  Lamp,
  Message,
  ModelInfo,
  QuotaMap,
  ToolMap,
  ToolStatus,
  MessageRole,
  MessageSource,
  MessagesPage,
  Project,
  Run,
  RunState,
  StatusInfo,
  TerminalSnapshot,
  TerminalSource,
  Turn,
  TurnDelivery,
  TurnOrigin,
  TurnStatus,
 TeamWorkerSpec, ProjectSubmodule } from './types'
import {
  BOT_KINDS,
  hostOfQuotaKey,
  TEAM_BUDGET_DEFAULTS,
  TEAM_PHASES,
  TEAM_ISSUE_STATES,
  TEAM_TASK_STATES,
  TEAM_USAGE_EMPTY,
  TOOL_UNKNOWN,
} from './types'

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
const TEAM_ROLES = ['pm', 'worker', 'reviewer'] as const
const TEAM_EVENT_KINDS = ['relay', 'phase', 'merge', 'note', 'user'] as const
const TEAM_EVENT_STATUSES = ['pending', 'delivered', 'dropped'] as const
const TEAM_DELIVERS = ['branch', 'pr'] as const

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
    connected: bool(pick(v, 'connected', 'ok', 'up'), false),
    error: optStr(pick(v, 'error', 'last_error', 'message', 'reason')),
    attach_command:
      str(pick(v, 'attach_command', 'attach')) ||
      `herdr --remote ${str(pick(v, 'ssh', 'target', 'ssh_target'))} --session ${str(pick(v, 'herdr_session', 'session'), 'agents-manager')}`,
    tools: toToolMap(pick(v, 'tools')),
    identity_status: toIdentityStatusMap(pick(v, 'identities', 'identity_status')),
  }
}

/**
 * v4.0 `hosts[].identities`：身份在這台主機上的登入狀態。缺欄位 / 讀不懂的一律當「未知」
 * （`logged_in: null`），絕不把問不到說成未登入。
 */
export function toIdentityStatusMap(raw: unknown): IdentityStatusMap {
  const root = isRec(raw) ? raw : {}
  const out: IdentityStatusMap = {}
  for (const [key, v] of Object.entries(root)) {
    if (!isRec(v)) continue
    const li = pick(v, 'logged_in', 'loggedIn')
    out[key] = {
      name: str(pick(v, 'name')) || key,
      kind: oneOf<BotKind>(v.kind, BOT_KINDS, 'claude'),
      logged_in: typeof li === 'boolean' ? li : null,
      account: optStr(pick(v, 'account', 'email')),
      plan: optStr(pick(v, 'plan', 'subscriptionType')),
      // 舊 daemon 沒有這兩個欄位：一律當成 config 來源（唯一會被編輯的那種）。
      source: str(pick(v, 'source')) === 'shell' ? 'shell' : 'config',
      config_dir: optStr(pick(v, 'config_dir')),
    }
  }
  return out
}

/** v4.0 `tools`：缺的 kind 視為未知（`TOOL_UNKNOWN`），不會誤報「缺少」。 */
export function toToolMap(raw: unknown): ToolMap {
  const root = isRec(raw) ? raw : {}
  const one = (v: unknown): ToolStatus => {
    if (!isRec(v)) return TOOL_UNKNOWN
    const li = pick(v, 'logged_in', 'loggedIn')
    return {
      installed: bool(pick(v, 'installed', 'ok'), true),
      path: optStr(pick(v, 'path')),
      version: optStr(pick(v, 'version')),
      logged_in: typeof li === 'boolean' ? li : null,
    }
  }
  const out = {} as ToolMap
  for (const k of BOT_KINDS) out[k] = one(root[k])
  return out
}

export function toInstallResult(raw: unknown): InstallToolResult {
  return { turn_id: isRec(raw) ? str(pick(raw, 'turn_id')) : '' }
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
    github: (() => {
      const g = pick(v, 'github')
      if (!isRec(g)) return null
      const owner = str(pick(g, 'owner'))
      const repo = str(pick(g, 'repo', 'name'))
      if (!owner || !repo) return null
      return { owner, repo, url: str(pick(g, 'url', 'html_url')) || `https://github.com/${owner}/${repo}` }
    })(),
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
    effort: optStr(pick(v, 'effort')),
    fast: bool(pick(v, 'fast'), false),
    persona: (() => {
      const x = pick(v, 'persona')
      return typeof x === 'string' && x.trim() ? x : null
    })(),
    args,
    autostart: bool(v.autostart),
    inject_hooks: bool(v.inject_hooks, true),
    auto_approve: bool(v.auto_approve, true),
    identity: (() => {
      const i = pick(v, 'identity')
      return typeof i === 'string' && i.trim() ? i : null
    })(),
    env: envMap(pick(v, 'env', 'env_json')),
    // SPEC-team §2.1：舊 daemon 沒有這些欄位 → `user` / null / null（行為與 team 之前相同）。
    managed_by: oneOf<BotManagedBy>(pick(v, 'managed_by'), ['user', 'team', 'child'], 'user'),
    team: toBotTeamRef(v),
    parent_bot_id: optStr(pick(v, 'parent_bot_id')),
    cwd: optStr(pick(v, 'cwd')),
    herdr_session: optStr(pick(v, 'herdr_session', 'session')),
    // debug 用的 herdr agent 名稱；舊 daemon 不送就是 null（UI 那顆晶片自己不渲染）。
    agent_name: optStr(pick(v, 'agent_name', 'agentName')),
    created_at: str(v.created_at),
  }
}

/**
 * `bots[].team` 可能是 `{team_id, role}`（API.md 形狀），也可能攤平成 `bots[].team_id` /
 * `bots[].team_role`（SQLite 直出）。
 *
 * 注意：**不能**把 `id` 當成 team_id 的別名——那是 bot 自己的主鍵，會讓每個一般 bot 都被
 * 誤判成 team 成員（sidebar 會整個空掉）。
 */
function toBotTeamRef(v: Rec): BotTeamRef | null {
  const nested = pick(v, 'team')
  if (isRec(nested)) {
    const teamId = optStr(pick(nested, 'team_id', 'id'))
    if (!teamId) return null
    return { team_id: teamId, role: oneOf<TeamRole>(pick(nested, 'role', 'team_role'), TEAM_ROLES, 'worker') }
  }
  const teamId = optStr(pick(v, 'team_id'))
  if (!teamId) return null
  return { team_id: teamId, role: oneOf<TeamRole>(pick(v, 'team_role'), TEAM_ROLES, 'worker') }
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

/** `runs.status_json` — a JSON string (or an already-parsed object) from the daemon. */
export function toStatusInfo(v: unknown): StatusInfo | null {
  let raw: unknown = v
  if (typeof raw === 'string') {
    if (!raw.trim()) return null
    try {
      raw = JSON.parse(raw) as unknown
    } catch {
      return null
    }
  }
  if (!isRec(raw)) return null
  const ctx = isRec(raw.context_window) ? raw.context_window : {}
  const limits = isRec(raw.rate_limits) ? raw.rate_limits : {}
  const five = isRec(limits.five_hour) ? limits.five_hour : {}
  const seven = isRec(limits.seven_day) ? limits.seven_day : {}
  const model = isRec(raw.model) ? raw.model : {}
  const cost = isRec(raw.cost) ? raw.cost : {}
  const workspace = isRec(raw.workspace) ? raw.workspace : {}
  const numOrNull = (x: unknown): number | null => (typeof x === 'number' && Number.isFinite(x) ? x : null)
  return {
    account_email: optStr(raw.account_email),
    account_warning: optStr(raw.account_warning),
    model_name: optStr(pick(model, 'display_name')),
    model_id: optStr(pick(model, 'id')),
    effort: optStr(isRec(raw.effort) ? pick(raw.effort, 'level') : undefined),
    thinking: isRec(raw.thinking) ? bool(pick(raw.thinking, 'enabled')) : false,
    fast_mode: bool(raw.fast_mode),
    context_used_pct: numOrNull(pick(ctx, 'used_percentage')),
    context_used_tokens: numOrNull(pick(ctx, 'total_input_tokens')),
    context_size: numOrNull(pick(ctx, 'context_window_size')),
    five_hour_pct: numOrNull(pick(five, 'used_percentage')),
    five_hour_resets_at: numOrNull(pick(five, 'resets_at')),
    seven_day_pct: numOrNull(pick(seven, 'used_percentage')),
    seven_day_resets_at: numOrNull(pick(seven, 'resets_at')),
    cost_usd: numOrNull(pick(cost, 'total_cost_usd')),
    cwd: optStr(pick(workspace, 'current_dir')) ?? optStr(raw.cwd),
    version: optStr(raw.version),
    session_name: optStr(raw.session_name),
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
    herdr_session: optStr(pick(v, 'herdr_session', 'session')),
    agent_title: optStr(pick(v, 'agent_title', 'agentTitle')),
    status_line: optStr(pick(v, 'status_line', 'statusLine')),
    status: toStatusInfo(pick(v, 'status_json', 'status')),
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

/**
 * `messages.attachments_json` — a JSON array the daemon stamps onto the user message.
 * Tolerates the parsed-array form too, in case the daemon ever inlines it.
 */
export function toAttachments(v: unknown): Attachment[] {
  let raw: unknown = v
  if (typeof raw === 'string') {
    if (!raw.trim()) return []
    try {
      raw = JSON.parse(raw) as unknown
    } catch {
      return []
    }
  }
  const out: Attachment[] = []
  for (const item of arr(raw)) {
    if (!isRec(item)) continue
    const id = str(item.id)
    if (!id) continue
    out.push({
      id,
      name: str(item.name, id),
      mime: str(item.mime, 'image/png'),
      size: num(item.size),
      path: str(item.path),
    })
  }
  return out
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
    group_id: optStr(pick(v, 'group_id', 'groupId')),
    attachments: toAttachments(pick(v, 'attachments_json', 'attachments')),
    team_id: optStr(pick(v, 'team_id', 'teamId')),
    relay_from: optStr(pick(v, 'relay_from', 'relayFrom')),
    terminal_snapshot: optStr(pick(v, 'terminal_snapshot', 'terminalSnapshot')),
    created_at: str(v.created_at),
  }
}

/** SPEC §13.4 group timeline row: a message that must know its bot. */
export function toGroupMessage(v: unknown): GroupMessage | null {
  const m = toMessage(v)
  if (!m || !isRec(v)) return null
  const bot_id = str(pick(v, 'bot_id', 'botId'))
  if (!bot_id) return null
  return { ...m, bot_id, bot_name: str(pick(v, 'bot_name', 'botName'), bot_id) }
}

export function toGroupMessagesPage(raw: unknown, projectId: string): GroupMessagesPage {
  const o = isRec(raw) ? raw : {}
  const out: GroupMessage[] = []
  for (const m of arr(pick(o, 'messages', 'items'))) {
    const gm = toGroupMessage(m)
    if (gm) out.push(gm)
  }
  return { project_id: str(pick(o, 'project_id'), projectId), messages: sortById(out), has_more: bool(pick(o, 'has_more')) }
}

/** Group timeline order = message id (ULID, time-ordered) — the daemon paginates by it. */
export function sortById<T extends { id: string }>(items: T[]): T[] {
  return [...items].sort((a, b) => a.id.localeCompare(b.id))
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
  const teams: Team[] = []
  const runs: Run[] = []
  const turns: Turn[] = []

  const session = str(pick(root, 'herdr_session'), 'agents-manager')
  let attachCommand = `herdr --session ${session}`
  let localTools = toToolMap(undefined)
  let localIdentityStatus: IdentityStatusMap = {}
  for (const h of hostArray(pick(root, 'hosts', 'host_list'))) {
    // API.md: `hosts[0]` is always the reserved `local` entry (ssh fields null). The UI
    // models the local machine separately (`state.connected`), so drop it here — keeping
    // only its v4.0 `attach_command`.
    const host = toHost(h)
    if (!host) continue
    if (host.name === 'local') {
      if (isRec(h) && str(pick(h, 'attach_command', 'attach'))) attachCommand = str(pick(h, 'attach_command', 'attach'))
      localTools = host.tools
      localIdentityStatus = host.identity_status
      continue
    }
    if (!hosts.some((x) => x.name === host.name)) hosts.push(host)
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
      // SPEC-team §10.2；舊 daemon 沒有 `teams` → 空陣列，UI 的 team 區塊自然消失。
      for (const t of arr(pick(p, 'teams', 'team_list'))) {
        const team = toTeam(t, project.id)
        if (team && !teams.some((x) => x.id === team.id)) teams.push(team)
      }
    }
  }
  for (const b of arr(pick(root, 'bots', 'bot_list'))) collectBot(b)
  for (const t of arr(pick(root, 'teams', 'team_list'))) {
    const team = toTeam(t)
    if (team && !teams.some((x) => x.id === team.id)) teams.push(team)
  }

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
    default_connected: bool(pick(root, 'default_connected'), false),
    attach_command: attachCommand,
    tools: localTools,
    identity_status: localIdentityStatus,
    hosts,
    identities,
    projects,
    bots,
    teams,
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
            : // 剛起來的 run 還沒有任何 hook／快照分類（實測 claude 要 ~20 秒才回第一個狀態）。
              // 這段期間標「狀態未知」會讓剛新增的 bot 看起來是壞的、像沒生出來；它其實正在開。
              // 只在起跑後 90 秒內這樣說，之後真的問不到狀態才是 `unknown`。
              justStarted(run)
              ? 'starting'
              : 'unknown'
  }
}

/** run 是不是剛起來（< 90 秒）。時間戳讀不出來時當作不是，寧可標 unknown 也不要一直說「啟動中」。 */
function justStarted(run: Run): boolean {
  const t = Date.parse(run.started_at)
  return Number.isNaN(t) ? false : Date.now() - t < 90_000
}

export function toTerminal(raw: unknown, source: TerminalSource): TerminalSnapshot {
  if (typeof raw === 'string')
    return { text: raw, revision: null, truncated: false, source, pane_id: null, columns: null, rows: null }
  const o = isRec(raw) ? raw : {}
  const textRaw = pick(o, 'text', 'content', 'data', 'output', 'snapshot', 'lines')
  const text = Array.isArray(textRaw) ? textRaw.map((l) => str(l)).join('\n') : str(textRaw)
  const rev = pick(o, 'revision', 'rev')
  return {
    text,
    revision: rev === undefined ? null : num(rev, 0),
    truncated: bool(o.truncated),
    source: oneOf<TerminalSource>(pick(o, 'source'), ['visible', 'recent_unwrapped'], source),
    pane_id: optStr(pick(o, 'pane_id')),
    // Absent on an older daemon: the UI must fall back to saying nothing, not to a wrong size.
    columns: o.columns === undefined || o.columns === null ? null : num(o.columns, 0) || null,
    rows: o.rows === undefined || o.rows === null ? null : num(o.rows, 0) || null,
  }
}

/** `POST /api/hosts/:name/shells` 的一列。`pane_id` 是空的就當這筆不存在（見 `toHostShells`）。 */
export function toHostShell(raw: unknown, fallbackHost: string): HostShell {
  const o = isRec(raw) ? raw : {}
  return {
    host: str(pick(o, 'host'), fallbackHost),
    pane_id: str(pick(o, 'pane_id')),
    tab_id: str(pick(o, 'tab_id')),
    workspace_id: str(pick(o, 'workspace_id')),
    cwd: str(pick(o, 'cwd')),
    created_at: str(pick(o, 'created_at')),
  }
}

/** `GET /api/hosts/:name/shells` → `{shells:[…]}`。沒有 `pane_id` 的列丟掉：整個面板都靠它定位。 */
export function toHostShells(raw: unknown, fallbackHost: string): HostShell[] {
  const o = isRec(raw) ? raw : {}
  return arr(o.shells)
    .map((s) => toHostShell(s, fallbackHost))
    .filter((s) => s.pane_id)
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

// ------------------------------------------------------------------ v4.0

/** `GET /api/models` → `ModelInfo[]`（缺欄位時給安全預設）。 */
export function toModels(raw: unknown): ModelInfo[] {
  const root = isRec(raw) ? raw : {}
  return arr(pick(root, 'models', 'items'))
    .filter(isRec)
    .map((m) => {
      const id = str(pick(m, 'id', 'name', 'model'))
      return {
        id,
        display_name: str(pick(m, 'display_name', 'label'), id),
        description: str(pick(m, 'description')),
        is_default: bool(pick(m, 'is_default', 'default'), false),
        default_effort: optStr(pick(m, 'default_effort')),
        efforts: arr(pick(m, 'efforts')).map((e) => str(e)).filter(Boolean),
        service_tiers: arr(pick(m, 'service_tiers', 'tiers'))
          .filter(isRec)
          .map((t) => ({ id: str(pick(t, 'id')), name: str(pick(t, 'name'), str(pick(t, 'id'))), description: str(pick(t, 'description')) })),
      }
    })
    .filter((m) => m.id)
}

function toQuotaWindow(v: unknown): KindQuota['five_hour'] {
  if (!isRec(v)) return null
  return {
    used_pct: Math.max(0, Math.min(100, num(pick(v, 'used_pct', 'used'), 0))),
    resets_at: optStr(pick(v, 'resets_at', 'reset_at')),
    // 門檻在 daemon 算好（見 docs/API.md §12.4）；舊 daemon 沒有這兩個欄位時預設 false，
    // 額度條退回「只有長條」的行為，不在前端自己補算。
    low: bool(pick(v, 'low'), false),
    critical: bool(pick(v, 'critical'), false),
  }
}

/**
 * 一個 kind 的額度；null 代表沒有資訊。
 * `key` 只用來補 `host`：舊 daemon 不送 `host`，就從 `m4p/claude` 這種 key 前綴推回來。
 */
export function toKindQuota(v: unknown, key?: string): KindQuota | null {
  if (!isRec(v)) return null
  return {
    five_hour: toQuotaWindow(pick(v, 'five_hour', '5h')),
    seven_day: toQuotaWindow(pick(v, 'seven_day', '7d')),
    // 舊 daemon 沒有這個欄位 → null，額度條就完全不畫 Fable 那條。
    fable: toQuotaWindow(pick(v, 'fable')),
    plan: optStr(pick(v, 'plan')),
    updated_at: str(pick(v, 'updated_at')),
    host: str(pick(v, 'host'), hostOfQuotaKey(key ?? '')),
  }
}

/** `GET /api/quota` → `{kinds: {...}}`（也接受直接給 map）。 */
export function toQuota(raw: unknown): QuotaMap {
  const root = isRec(raw) ? raw : {}
  const kinds = isRec(root.kinds) ? root.kinds : root
  const out: QuotaMap = {}
  for (const [k, v] of Object.entries(kinds)) out[k] = toKindQuota(v, k)
  return out
}

// ------------------------------------------------------------------ v4.0 issues

function toLabel(v: unknown): IssueLabel | null {
  if (typeof v === 'string') return v ? { name: v, color: null } : null
  if (!isRec(v)) return null
  const name = str(pick(v, 'name', 'label'))
  if (!name) return null
  const c = str(pick(v, 'color')).replace(/^#/, '')
  return { name, color: /^[0-9a-fA-F]{6}$/.test(c) ? c : null }
}

export function toIssue(v: unknown): Issue | null {
  if (!isRec(v)) return null
  const number = num(pick(v, 'number', 'id'), 0)
  if (!number) return null
  const state = str(pick(v, 'state'), 'open').toLowerCase()
  return {
    number,
    title: str(pick(v, 'title')),
    state: state === 'closed' ? 'closed' : 'open',
    labels: arr(pick(v, 'labels')).map(toLabel).filter((l): l is IssueLabel => l !== null),
    url: str(pick(v, 'url', 'html_url')),
    updated_at: str(pick(v, 'updated_at', 'updatedAt')),
    author: (() => {
      const a = pick(v, 'author', 'user')
      return isRec(a) ? str(pick(a, 'login', 'name')) : str(a)
    })(),
    body_excerpt: str(pick(v, 'body_excerpt', 'excerpt')),
  }
}

export function toSubmodules(raw: unknown): ProjectSubmodule[] {
  const root = isRec(raw) ? raw : {}
  const out: ProjectSubmodule[] = []
  for (const v of arr(pick(root, 'submodules'))) {
    if (!isRec(v)) continue
    const path = str(v.path)
    if (!path) continue
    const g = v.github
    const github =
      isRec(g) && str(g.owner) && str(g.repo) ? { owner: str(g.owner), repo: str(g.repo), url: str(g.url) } : null
    out.push({ path, github })
  }
  return out
}

export function toIssues(raw: unknown): Issue[] {
  const root = isRec(raw) ? raw : {}
  return arr(pick(root, 'issues', 'items')).map(toIssue).filter((i): i is Issue => i !== null)
}

export function toIssueDetail(raw: unknown): IssueDetail | null {
  const root = isRec(raw) && isRec(root_issue(raw)) ? root_issue(raw) : raw
  const base = toIssue(root)
  if (!base) return null
  return { ...base, body: isRec(root) ? str(pick(root, 'body')) : '' }
}

function root_issue(raw: Record<string, unknown>): unknown {
  return raw.issue ?? raw
}

// -------------------------------------------------------- Issue Team（SPEC-team §10）

export function toTeamBudget(v: unknown): TeamBudget {
  const o = isRec(v) ? v : {}
  return {
    max_relays: num(pick(o, 'max_relays'), TEAM_BUDGET_DEFAULTS.max_relays),
    max_review_rounds: num(pick(o, 'max_review_rounds'), TEAM_BUDGET_DEFAULTS.max_review_rounds),
    max_wall_clock_min: num(pick(o, 'max_wall_clock_min'), TEAM_BUDGET_DEFAULTS.max_wall_clock_min),
    quota_stop_pct: num(pick(o, 'quota_stop_pct'), TEAM_BUDGET_DEFAULTS.quota_stop_pct),
  }
}

export function toTeamUsage(v: unknown): TeamUsage {
  if (!isRec(v)) return { ...TEAM_USAGE_EMPTY, per_bot: {} }
  const perBot: TeamUsage['per_bot'] = {}
  const raw = pick(v, 'per_bot')
  if (isRec(raw)) {
    for (const [k, val] of Object.entries(raw)) perBot[k] = { turns: num(isRec(val) ? pick(val, 'turns') : val, 0) }
  }
  return {
    relays: num(pick(v, 'relays'), 0),
    review_rounds_total: num(pick(v, 'review_rounds_total'), 0),
    elapsed_min: num(pick(v, 'elapsed_min'), 0),
    per_bot: perBot,
  }
}

function toTasksSummary(v: unknown): TeamTasksSummary {
  const o = isRec(v) ? v : {}
  const out: TeamTasksSummary = { total: num(pick(o, 'total'), 0) }
  for (const st of TEAM_TASK_STATES) {
    if (o[st] !== undefined) out[st] = num(o[st], 0)
  }
  return out
}

function toMembers(v: unknown): TeamMember[] {
  const out: TeamMember[] = []
  for (const m of arr(v)) {
    if (!isRec(m)) continue
    const botId = str(pick(m, 'bot_id', 'id'))
    if (!botId) continue
    out.push({
      bot_id: botId,
      role: oneOf<TeamRole>(pick(m, 'role', 'team_role'), TEAM_ROLES, 'worker'),
      deleted: bool(pick(m, 'deleted'), false),
    })
  }
  return out
}

export function toTeam(v: unknown, projectId?: string): Team | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'team_id'))
  if (!id) return null
  return {
    id,
    project_id: str(pick(v, 'project_id'), projectId ?? ''),
    issue_number: num(pick(v, 'issue_number', 'issue'), 0),
    issue_title: str(pick(v, 'issue_title', 'title')),
    issue_url: str(pick(v, 'issue_url', 'url')),
    // §2.3：舊 daemon 不送這三個，佇列就退化成「只有當前這一個 issue」。
    issues: arr(pick(v, 'issues')).map(toTeamIssue).filter((x): x is TeamIssue => x !== null),
    current_issue_id: optStr(pick(v, 'current_issue_id')),
    issues_summary: toIssuesSummary(pick(v, 'issues_summary')),
    phase: oneOf<TeamPhase>(pick(v, 'phase'), TEAM_PHASES, 'starting'),
    pause_reason: optStr(pick(v, 'pause_reason')),
    branch: str(pick(v, 'branch')),
    deliver: oneOf<TeamDeliver>(pick(v, 'deliver'), TEAM_DELIVERS, 'branch'),
    supervised: bool(pick(v, 'supervised'), false),
    members: toMembers(pick(v, 'members')),
    tasks_summary: toTasksSummary(pick(v, 'tasks_summary')),
    budget: toTeamBudget(pick(v, 'budget', 'budget_json')),
    usage: toTeamUsage(pick(v, 'usage', 'usage_json')),
    pr_url: optStr(pick(v, 'pr_url')),
    issue_closed_at: optStr(pick(v, 'issue_closed_at')),
    repo: str(pick(v, 'repo')),
    created_at: str(pick(v, 'created_at')),
    started_at: optStr(pick(v, 'started_at')),
    ended_at: optStr(pick(v, 'ended_at')),
  }
}

export function toTeamIssue(v: unknown): TeamIssue | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id'))
  if (!id) return null
  return {
    id,
    seq: num(pick(v, 'seq'), 0),
    issue_number: num(pick(v, 'issue_number'), 0),
    issue_title: str(pick(v, 'issue_title')),
    issue_url: str(pick(v, 'issue_url')),
    state: oneOf<TeamIssueState>(pick(v, 'state'), TEAM_ISSUE_STATES, 'queued'),
    branch: optStr(pick(v, 'branch')),
    summary: optStr(pick(v, 'summary')),
    pr_url: optStr(pick(v, 'pr_url')),
    issue_closed_at: optStr(pick(v, 'issue_closed_at')),
    fail_reason: optStr(pick(v, 'fail_reason')),
    started_at: optStr(pick(v, 'started_at')),
    ended_at: optStr(pick(v, 'ended_at')),
  }
}

function toIssuesSummary(v: unknown): TeamIssuesSummary {
  const r = isRec(v) ? v : {}
  return {
    total: num(pick(r, 'total'), 0),
    done: num(pick(r, 'done'), 0),
    failed: num(pick(r, 'failed'), 0),
    queued: num(pick(r, 'queued'), 0),
  }
}

export function toTeamTask(v: unknown): TeamTask | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'task_id'))
  if (!id) return null
  return {
    id,
    issue_id: optStr(pick(v, 'issue_id')),
    seq: num(pick(v, 'seq'), 0),
    title: str(pick(v, 'title')),
    brief: str(pick(v, 'brief')),
    files: arr(pick(v, 'files', 'files_json')).map((f) => str(f)).filter(Boolean),
    worker_bot_id: str(pick(v, 'worker_bot_id', 'bot_id')),
    branch: str(pick(v, 'branch')),
    state: oneOf<TeamTaskState>(pick(v, 'state'), TEAM_TASK_STATES, 'queued'),
    round: num(pick(v, 'round'), 0),
    last_report: optStr(pick(v, 'last_report')),
    last_verdict: optStr(pick(v, 'last_verdict')),
    merge_sha: optStr(pick(v, 'merge_sha')),
    updated_at: str(pick(v, 'updated_at')),
  }
}

export function toTeamDetail(raw: unknown, teamId: string): TeamDetail | null {
  const root = isRec(raw) && isRec(raw.team) ? raw.team : raw
  const base = toTeam(root)
  if (!base) return null
  const o = isRec(root) ? root : {}
  const outer = isRec(raw) ? raw : {}
  const tasks: TeamTask[] = []
  for (const t of arr(pick(o, 'tasks') ?? pick(outer, 'tasks'))) {
    const task = toTeamTask(t)
    if (task) tasks.push(task)
  }
  return {
    ...base,
    id: base.id || teamId,
    tasks: tasks.sort((a, b) => a.seq - b.seq),
    summary: optStr(pick(o, 'summary') ?? pick(outer, 'summary')),
    base_ref: str(pick(o, 'base_ref') ?? pick(outer, 'base_ref'), 'HEAD'),
    base_sha: str(pick(o, 'base_sha') ?? pick(outer, 'base_sha')),
    worktree_root: str(pick(o, 'worktree_root') ?? pick(outer, 'worktree_root')),
    workers: toWorkerSpec(pick(o, 'roles') ?? pick(outer, 'roles')),
  }
}

function toWorkerSpec(roles: unknown): TeamWorkerSpec | null {
  if (!isRec(roles) || !isRec(roles.workers) || !isRec(roles.workers.spec)) return null
  const sp = roles.workers.spec
  const kind = str(sp.kind)
  if (kind !== 'claude' && kind !== 'codex' && kind !== 'grok') return null
  return {
    kind,
    model: optStr(sp.model),
    effort: optStr(sp.effort),
    fast: sp.fast === true,
    identity: optStr(sp.identity),
  }
}

export function toTeamEvent(v: unknown): TeamEvent | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'event_id'))
  if (!id) return null
  const payload = pick(v, 'payload', 'payload_json')
  let parsed: unknown = payload
  if (typeof payload === 'string') {
    try {
      parsed = JSON.parse(payload)
    } catch {
      parsed = { text: payload }
    }
  }
  return {
    id,
    kind: oneOf<TeamEventKind>(pick(v, 'kind'), TEAM_EVENT_KINDS, 'note'),
    from_bot_id: optStr(pick(v, 'from_bot_id')),
    to_bot_id: optStr(pick(v, 'to_bot_id')),
    task_id: optStr(pick(v, 'task_id')),
    turn_id: optStr(pick(v, 'turn_id')),
    status: (() => {
      const s = pick(v, 'status')
      return s === undefined ? null : oneOf<TeamEventStatus>(s, TEAM_EVENT_STATUSES, 'delivered')
    })(),
    payload: isRec(parsed) ? parsed : {},
    created_at: str(pick(v, 'created_at')),
  }
}

export function toTeamEvents(raw: unknown): TeamEvent[] {
  const root = isRec(raw) ? raw : {}
  const list = Array.isArray(raw) ? raw : arr(pick(root, 'events', 'items'))
  const out: TeamEvent[] = []
  for (const e of list) {
    const ev = toTeamEvent(e)
    if (ev) out.push(ev)
  }
  return sortById(out)
}


/** daemon 之外的字串一律當 `unknown`：多一個不認得的分類會讓「可以砍嗎」變成猜的。 */
function toOwner(v: unknown): MemOwner {
  const s = str(v)
  return s === 'bot' || s === 'pane' || s === 'herdr' ? s : 'unknown'
}

/** `GET /api/mem/processes`（SPEC §15.2）。舊 daemon 沒有這支 → 空清單。 */
export function toMemProcesses(v: unknown): MemProcesses {
  const r = isRec(v) ? v : {}
  const rows: MemProcess[] = arr(r.processes)
    .filter(isRec)
    .map((p) => ({
      pid: num(p.pid),
      ppid: num(p.ppid),
      rss_bytes: num(p.rss_bytes),
      exe: str(p.exe),
      argv: str(p.argv),
      pane_id: p.pane_id == null ? null : str(p.pane_id),
      socket_path: p.socket_path == null ? null : str(p.socket_path),
      bot_id: p.bot_id == null ? null : str(p.bot_id),
      bot_name: p.bot_name == null ? null : str(p.bot_name),
      project_id: p.project_id == null ? null : str(p.project_id),
      owner: toOwner(p.owner),
      subtree_bytes: num(p.subtree_bytes),
      children: num(p.children),
    }))
  return { host: str(r.host), sampled_at: str(r.sampled_at), processes: rows }
}

/** `GET /api/mem` / WS `mem_updated`（SPEC §15）。舊 daemon 沒有這支 → 全 0，UI 就不顯示。 */
export function toMemSnapshot(v: unknown): MemSnapshot {
  const r = isRec(v) ? v : {}
  const n = (x: unknown): number => (typeof x === 'number' && Number.isFinite(x) && x >= 0 ? x : 0)
  const hosts = (Array.isArray(r.hosts) ? r.hosts : []).filter(isRec).map((h) => ({
    host: str(h.host),
    herdr_bytes: n(h.herdr_bytes),
    agents_bytes: n(h.agents_bytes),
    total_bytes: n(h.total_bytes),
    processes: n(h.processes),
    error: h.error == null ? null : str(h.error),
  }))
  return {
    total_bytes: n(r.total_bytes),
    herdr_bytes: n(r.herdr_bytes),
    agents_bytes: n(r.agents_bytes),
    processes: n(r.processes),
    hosts,
  }
}
