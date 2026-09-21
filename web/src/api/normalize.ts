/** Tolerant decoders for daemon payloads: accept plausible shapes (SQLite 0/1 booleans, JSON-string fields, nested or flat bots/runs). */

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
  InstructionFiles,
  GroupMessage,
  GroupMessagesPage,
  HostShell,
  Host,
  HerdrVersion,
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
  QuotaLimitHit,
  QuotaResetCredits,
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
 ProjectSubmodule } from './types.ts'
import {
  UNKNOWN_HERDR,
  BOT_KINDS,
  INSTRUCTION_FILES,
  type Mission,
  type MissionAssignment,
  type MissionParentRef,
  type MissionRevisionRef,
  type MissionDelivery,
  type MissionDetail,
  type MissionEvent,
  type MissionEventKind,
  type MissionOn5h,
  type MissionPhaseServer,
  type MissionRole,
  type MissionStatus,
  hostOfQuotaKey,
  TOOL_UNKNOWN,
} from './types.ts'

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
const TURN_STATUSES = ['queued', 'in_flight', 'completed', 'completed_fallback', 'failed'] as const
const DELIVERIES = ['pending', 'ok', 'unknown', 'failed'] as const
const ORIGINS = ['web', 'external'] as const
const ROLES = ['user', 'assistant', 'system'] as const
const SOURCES = ['web', 'hook', 'transcript', 'terminal_fallback', 'system'] as const

/** 缺欄位＝舊 daemon，一律當未知（null），不猜。 */
export function toHerdrVersion(v: unknown): HerdrVersion {
  if (!isRec(v)) return UNKNOWN_HERDR
  const nullableStr = (x: unknown) => (typeof x === 'string' && x.trim() ? x : null)
  const protocol = pick(v, 'protocol')
  const supported = pick(v, 'protocol_supported')
  return {
    server_version: nullableStr(pick(v, 'server_version')),
    protocol: typeof protocol === 'number' ? protocol : null,
    protocol_supported: typeof supported === 'boolean' ? supported : null,
    cli_version: nullableStr(pick(v, 'cli_version')),
    mismatch: pick(v, 'mismatch') === true,
  }
}

/** SPEC §11.6 */
export function toHost(v: unknown): Host | null {
  if (!isRec(v)) return null
  const name = str(pick(v, 'name'))
  if (!name) return null
  return {
    name,
    ssh: str(pick(v, 'ssh')),
    ssh_port: num(pick(v, 'ssh_port'), 22),
    herdr_session: str(pick(v, 'herdr_session'), 'agents-manager'),
    remote_path: str(pick(v, 'remote_path')),
    connected: bool(pick(v, 'connected'), false),
    error: optStr(pick(v, 'error')),
    attach_command:
      str(pick(v, 'attach_command')) ||
      `herdr --remote ${str(pick(v, 'ssh'))} --session ${str(pick(v, 'herdr_session'), 'agents-manager')}`,
    herdr: toHerdrVersion(pick(v, 'herdr')),
    tools: toToolMap(pick(v, 'tools')),
    identity_status: toIdentityStatusMap(pick(v, 'identities')),
  }
}

/** 讀不懂的一律 `logged_in: null`，絕不把問不到說成未登入。 */
export function toIdentityStatusMap(raw: unknown): IdentityStatusMap {
  const root = isRec(raw) ? raw : {}
  const out: IdentityStatusMap = {}
  for (const [key, v] of Object.entries(root)) {
    if (!isRec(v)) continue
    const li = pick(v, 'logged_in')
    out[key] = {
      name: str(pick(v, 'name')) || key,
      kind: oneOf<BotKind>(v.kind, BOT_KINDS, 'claude'),
      logged_in: typeof li === 'boolean' ? li : null,
      reason: optStr(pick(v, 'reason')),
      account: optStr(pick(v, 'account')),
      plan: optStr(pick(v, 'plan')),
      source: str(pick(v, 'source')) === 'shell' ? 'shell' : 'config',
      config_dir: optStr(pick(v, 'config_dir')),
    }
  }
  return out
}

/** 缺的 kind 視為未知，不誤報「缺少」。 */
export function toToolMap(raw: unknown): ToolMap {
  const root = isRec(raw) ? raw : {}
  const one = (v: unknown): ToolStatus => {
    if (!isRec(v)) return TOOL_UNKNOWN
    const li = pick(v, 'logged_in')
    return {
      installed: bool(pick(v, 'installed'), true),
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

/** Array in `GET /api/state`, `{name: {...}}` map in `daemon_status` (SPEC §11.6). */
export function hostArray(v: unknown): unknown[] {
  if (Array.isArray(v)) return v
  if (isRec(v)) return Object.entries(v).map(([name, val]) => (isRec(val) ? { name, ...val } : { name }))
  return []
}

export function toProject(v: unknown): Project | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id'))
  if (!id) return null
  const path = str(pick(v, 'path'))
  return {
    id,
    path,
    label: str(pick(v, 'label'), path.split('/').pop() ?? id),
    workspace_id: optStr(v.workspace_id),
    host: str(pick(v, 'host'), 'local') || 'local',
    github: (() => {
      const g = pick(v, 'github')
      if (!isRec(g)) return null
      const owner = str(pick(g, 'owner'))
      const repo = str(pick(g, 'repo'))
      if (!owner || !repo) return null
      return { owner, repo, url: str(pick(g, 'url')) || `https://github.com/${owner}/${repo}` }
    })(),
    created_at: str(v.created_at),
  }
}

export function toBot(v: unknown, projectId?: string): Bot | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id'))
  if (!id) return null
  const args = arr(pick(v, 'args')).map((a) => str(a))
  return {
    id,
    project_id: str(pick(v, 'project_id'), projectId ?? ''),
    name: str(pick(v, 'name'), id),
    kind: oneOf<BotKind>(v.kind, BOT_KINDS, 'claude'),
    model: optStr(pick(v, 'model')),
    effort: optStr(pick(v, 'effort')),
    fast: bool(pick(v, 'fast'), false),
    persona: (() => {
      const x = pick(v, 'persona')
      return typeof x === 'string' && x.trim() ? x : null
    })(),
    // 只有 claude 有；daemon 沒給（codex／grok、舊版）或給了看不懂的值＝null，面板就不顯示這一格，不猜。
    instruction_files: (() => {
      const x = pick(v, 'instruction_files')
      return typeof x === 'string' && (INSTRUCTION_FILES as readonly string[]).includes(x) ? (x as InstructionFiles) : null
    })(),
    args,
    autostart: bool(v.autostart),
    inject_hooks: bool(v.inject_hooks, true),
    auto_approve: bool(v.auto_approve, true),
    identity: (() => {
      const i = pick(v, 'identity')
      return typeof i === 'string' && i.trim() ? i : null
    })(),
    env: envMap(pick(v, 'env')),
    managed_by: oneOf<BotManagedBy>(pick(v, 'managed_by'), ['user', 'child'], 'user'),
    parent_bot_id: optStr(pick(v, 'parent_bot_id')),
    primary: bool(pick(v, 'primary')),
    needs_restart: bool(pick(v, 'needs_restart')),
    primary_position: (() => {
      const n = pick(v, 'primary_position')
      return typeof n === 'number' && Number.isFinite(n) && n >= 0 ? Math.floor(n) : 0
    })(),
    cwd: optStr(pick(v, 'cwd')),
    ...(() => {
      const x = pick(v, 'preview')
      if (x === undefined) return {}
      if (!isRec(x)) return { preview: null }
      const st = pick(x, 'status')
      const port = pick(x, 'port')
      return {
        preview: {
          status: st === 'starting' || st === 'running' || st === 'failed' ? st : ('off' as const),
          port: typeof port === 'number' && port > 0 ? Math.floor(port) : null,
        },
      }
    })(),
    herdr_session: optStr(pick(v, 'herdr_session')),
    agent_name: optStr(pick(v, 'agent_name')),
    ...(() => {
      const n = pick(v, 'unread')
      const m = pick(v, 'read_mark')
      return {
        unread: typeof n === 'number' && Number.isFinite(n) && n >= 0 ? Math.floor(n) : undefined,
        read_mark: isRec(m) && typeof m.at === 'string' && m.at ? { at: m.at, id: typeof m.id === 'string' ? m.id : '' } : null,
      }
    })(),
    created_at: str(v.created_at),
  }
}

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
  const host = str(pick(v, 'host'))
  return {
    name,
    kind: oneOf<BotKind>(v.kind, BOT_KINDS, 'claude'),
    host: host || null,
    env: envMap(pick(v, 'env')),
    args: Array.isArray(rawArgs) ? rawArgs.map((a) => str(a)) : [],
  }
}

/** `runs.status_json` */
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
  const id = str(pick(v, 'id'))
  const bot_id = str(pick(v, 'bot_id'), botId ?? '')
  if (!id || !bot_id) return null
  return {
    id,
    bot_id,
    state: oneOf<RunState>(pick(v, 'state'), RUN_STATES, 'running'),
    agent_status: oneOf<AgentStatus>(pick(v, 'agent_status'), AGENT_STATUSES, 'unknown'),
    workspace_id: optStr(v.workspace_id),
    pane_id: optStr(v.pane_id),
    adopted: bool(v.adopted),
    herdr_session: optStr(pick(v, 'herdr_session')),
    agent_title: optStr(pick(v, 'agent_title')),
    status_line: optStr(pick(v, 'status_line')),
    status: toStatusInfo(pick(v, 'status_json')),
    update_notice: optStr(pick(v, 'update_notice')),
    turn_error: optStr(pick(v, 'turn_error')),
    // SPEC §4.4a：null = daemon 不知道，不能當 false
    runtime_model: optStr(pick(v, 'runtime_model')),
    runtime_effort: optStr(pick(v, 'runtime_effort')),
    runtime_fast: pick(v, 'runtime_fast') == null ? null : bool(pick(v, 'runtime_fast')),
    // 空字串是 daemon 明確記下的本機預設身份，不能用 optStr 折成「未知」的 null。
    runtime_identity: typeof pick(v, 'runtime_identity') === 'string' ? (pick(v, 'runtime_identity') as string) : null,
    native_session_id: optStr(v.native_session_id),
    transcript_path: optStr(v.transcript_path),
    started_at: str(v.started_at),
    ended_at: optStr(v.ended_at),
    agent_status_since: optStr(pick(v, 'agent_status_since')),
  }
}

export function toTurn(v: unknown, botId?: string): Turn | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id'))
  if (!id) return null
  return {
    id,
    conversation_id: str(v.conversation_id),
    run_id: optStr(v.run_id),
    bot_id: optStr(v.bot_id) ?? botId ?? null,
    origin: oneOf<TurnOrigin>(v.origin, ORIGINS, 'web'),
    // 未知 status 當終態：daemon 新增終態時退回 in_flight 會把輸入框鎖進排隊模式。
    status: oneOf<TurnStatus>(v.status, TURN_STATUSES, 'failed'),
    delivery: oneOf<TurnDelivery>(v.delivery, DELIVERIES, 'pending'),
    unverified: v.delivery_verified === 0,
    // 舊 daemon 沒有這一欄：當作會重送，才不會把每一則都標成「沒人會再試」。
    autoResend: v.auto_resend !== 0,
    awaitsStart: v.awaits_start === 1 || v.awaits_start === true,
    startError: optStr(v.start_error),
    client_request_id: optStr(v.client_request_id),
    created_at: str(v.created_at),
    completed_at: optStr(v.completed_at),
  }
}

/** `messages.attachments_json` (string or parsed array). */
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
  const id = str(pick(v, 'id'))
  if (!id) return null
  return {
    id,
    conversation_id: str(v.conversation_id),
    turn_id: optStr(v.turn_id),
    bot_id: optStr(v.bot_id) ?? botId ?? null,
    role: oneOf<MessageRole>(v.role, ROLES, 'system'),
    content: str(pick(v, 'content')),
    source: oneOf<MessageSource>(v.source, SOURCES, 'system'),
    incomplete: bool(v.incomplete),
    group_id: optStr(pick(v, 'group_id')),
    attachments: toAttachments(pick(v, 'attachments_json')),
    relay_from: optStr(pick(v, 'relay_from')),
    terminal_snapshot: optStr(pick(v, 'terminal_snapshot')),
    created_at: str(v.created_at),
  }
}

/** SPEC §13.4 */
export function toGroupMessage(v: unknown): GroupMessage | null {
  const m = toMessage(v)
  if (!m || !isRec(v)) return null
  const bot_id = str(pick(v, 'bot_id'))
  if (!bot_id) return null
  return { ...m, bot_id, bot_name: str(pick(v, 'bot_name'), bot_id) }
}

export function toGroupMessagesPage(raw: unknown, projectId: string): GroupMessagesPage {
  const o = isRec(raw) ? raw : {}
  const out: GroupMessage[] = []
  for (const m of arr(pick(o, 'messages'))) {
    const gm = toGroupMessage(m)
    if (gm) out.push(gm)
  }
  return { project_id: str(pick(o, 'project_id'), projectId), messages: sortById(out), has_more: bool(pick(o, 'has_more')) }
}

/** ULID order — the daemon paginates by it. */
export function sortById<T extends { id: string }>(items: T[]): T[] {
  return [...items].sort((a, b) => a.id.localeCompare(b.id))
}

export function sortByTime<T extends { created_at: string; id: string }>(items: T[]): T[] {
  return [...items].sort((a, b) => {
    const t = a.created_at.localeCompare(b.created_at)
    return t !== 0 ? t : a.id.localeCompare(b.id)
  })
}

/**
 * Same, but same-millisecond items keep the order the daemon sent them in.
 *
 * Mission events already come back in write order (`ORDER BY created_at, rowid`); `created_at` only
 * has millisecond resolution and the ids are ULIDs, whose random section is not monotonic inside one
 * millisecond, so tie-breaking on `id` here would scramble a round/paused/resumed burst all over again.
 * `Array.prototype.sort` is stable, so returning 0 for a tie keeps the incoming order.
 */
export function sortByTimeStable<T extends { created_at: string }>(items: T[]): T[] {
  return [...items].sort((a, b) => a.created_at.localeCompare(b.created_at))
}

export function toState(raw: unknown): AppState {
  const root = isRec(raw) ? raw : {}
  const hosts: Host[] = []
  const identities: Identity[] = []
  const projects: Project[] = []
  const bots: Bot[] = []
  const runs: Run[] = []
  const turns: Turn[] = []

  const session = str(pick(root, 'herdr_session'), 'agents-manager')
  let attachCommand = `herdr --session ${session}`
  let localTools = toToolMap(undefined)
  let localHerdr = UNKNOWN_HERDR
  let localIdentityStatus: IdentityStatusMap = {}
  for (const h of hostArray(pick(root, 'hosts'))) {
    // `local` is modelled via `state.connected`; keep only its attach/tools/identities.
    const host = toHost(h)
    if (!host) continue
    if (host.name === 'local') {
      if (isRec(h) && str(h.attach_command)) attachCommand = str(h.attach_command)
      localTools = host.tools
      localHerdr = host.herdr
      localIdentityStatus = host.identity_status
      continue
    }
    if (!hosts.some((x) => x.name === host.name)) hosts.push(host)
  }

  for (const i of arr(pick(root, 'identities'))) {
    const ident = toIdentity(i)
    if (ident && !identities.some((x) => x.name === ident.name)) identities.push(ident)
  }

  for (const p of arr(pick(root, 'projects'))) {
    const project = toProject(p)
    if (!project) continue
    projects.push(project)
    if (isRec(p)) {
      for (const b of arr(pick(p, 'bots'))) collectBot(b, project.id)
    }
  }
  for (const b of arr(pick(root, 'bots'))) collectBot(b)

  for (const r of arr(pick(root, 'runs'))) {
    const run = toRun(r)
    if (run) runs.push(run)
  }
  for (const t of arr(pick(root, 'turns'))) {
    const turn = toTurn(t)
    if (turn) turns.push(turn)
  }

  function collectBot(b: unknown, projectId?: string) {
    const bot = toBot(b, projectId)
    if (!bot || bots.some((x) => x.id === bot.id)) return
    bots.push(bot)
    if (isRec(b)) {
      const embedded = pick(b, 'run')
      const run = toRun(embedded, bot.id)
      if (run && !runs.some((x) => x.id === run.id)) runs.push(run)
      const t = toTurn(pick(b, 'in_flight_turn'), bot.id)
      if (t && !turns.some((x) => x.id === t.id)) turns.push(t)
      const queued = toTurn(pick(b, 'queued_turn'), bot.id)
      if (queued && !turns.some((x) => x.id === queued.id)) turns.push(queued)
    }
  }

  return {
    daemon_seq: num(pick(root, 'daemon_seq'), 0),
    connected: bool(pick(root, 'connected'), true),
    default_connected: bool(pick(root, 'default_connected'), false),
    attach_command: attachCommand,
    herdr: localHerdr,
    tools: localTools,
    identity_status: localIdentityStatus,
    hosts,
    identities,
    projects,
    bots,
    runs,
    turns,
  }
}

export function toMessages(raw: unknown, botId: string): Message[] {
  const list = Array.isArray(raw) ? raw : isRec(raw) ? arr(pick(raw, 'messages')) : []
  const out: Message[] = []
  for (const m of list) {
    const msg = toMessage(m, botId)
    if (msg) out.push(msg)
  }
  // SPEC §7.2: endpoint is descending; UI renders ascending.
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

/** SPEC §2.2 */
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
            : // 實測 claude ~20 秒才回第一個狀態；90 秒內標 unknown 會像 bot 沒生出來。
              justStarted(run)
              ? 'starting'
              : 'unknown'
  }
}

/** 時間戳讀不出來時當作不是，寧可 unknown 也不要一直說「啟動中」。 */
function justStarted(run: Run): boolean {
  const t = Date.parse(run.started_at)
  return Number.isNaN(t) ? false : Date.now() - t < 90_000
}

export function toTerminal(raw: unknown, source: TerminalSource): TerminalSnapshot {
  if (typeof raw === 'string')
    return { text: raw, revision: null, truncated: false, source, pane_id: null, columns: null, rows: null }
  const o = isRec(raw) ? raw : {}
  const text = str(pick(o, 'text'))
  const rev = pick(o, 'revision')
  return {
    text,
    revision: rev === undefined ? null : num(rev, 0),
    truncated: bool(o.truncated),
    source: oneOf<TerminalSource>(pick(o, 'source'), ['visible', 'recent_unwrapped'], source),
    pane_id: optStr(pick(o, 'pane_id')),
    // null rather than a wrong size
    columns: o.columns === undefined || o.columns === null ? null : num(o.columns, 0) || null,
    rows: o.rows === undefined || o.rows === null ? null : num(o.rows, 0) || null,
  }
}

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

/** 沒有 `pane_id` 的列丟掉：面板靠它定位。 */
export function toHostShells(raw: unknown, fallbackHost: string): HostShell[] {
  const o = isRec(raw) ? raw : {}
  return arr(o.shells)
    .map((s) => toHostShell(s, fallbackHost))
    .filter((s) => s.pane_id)
}

export function frameBotId(data: unknown): string | null {
  if (!isRec(data)) return null
  const direct = optStr(pick(data, 'bot_id'))
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

export function unwrap(data: unknown, key: string): unknown {
  if (isRec(data) && isRec(data[key])) return data[key]
  return data
}

export { isRec, num, str, bool, optStr, pick, arr }

export function toModels(raw: unknown): ModelInfo[] {
  const root = isRec(raw) ? raw : {}
  return arr(pick(root, 'models'))
    .filter(isRec)
    .map((m) => {
      const id = str(pick(m, 'id'))
      return {
        id,
        display_name: str(pick(m, 'display_name'), id),
        description: str(pick(m, 'description')),
        is_default: bool(pick(m, 'is_default'), false),
        default_effort: optStr(pick(m, 'default_effort')),
        efforts: arr(pick(m, 'efforts')).map((e) => str(e)).filter(Boolean),
        service_tiers: arr(pick(m, 'service_tiers'))
          .filter(isRec)
          .map((t) => ({ id: str(pick(t, 'id')), name: str(pick(t, 'name'), str(pick(t, 'id'))), description: str(pick(t, 'description')) })),
      }
    })
    .filter((m) => m.id)
}

function toQuotaWindow(v: unknown): KindQuota['five_hour'] {
  if (!isRec(v)) return null
  return {
    used_pct: Math.max(0, Math.min(100, num(pick(v, 'used_pct'), 0))),
    resets_at: optStr(pick(v, 'resets_at')),
    // 門檻由 daemon 算（API.md §12.4），前端不補算
    low: bool(pick(v, 'low'), false),
    critical: bool(pick(v, 'critical'), false),
  }
}

function toResetCredits(v: unknown): QuotaResetCredits | null {
  if (!isRec(v)) return null
  const available = num(pick(v, 'available'), 0)
  return { available, title: optStr(pick(v, 'title')), expires_at: optStr(pick(v, 'expires_at')) }
}

function toLimitHit(v: unknown): QuotaLimitHit | null {
  if (!isRec(v)) return null
  const message = str(pick(v, 'message'))
  if (!message) return null
  return { message, until: optStr(pick(v, 'until')), at: str(pick(v, 'at')) }
}

/** `key` 只用來在缺 `host` 時從前綴推回。 */
export function toKindQuota(v: unknown, key?: string): KindQuota | null {
  if (!isRec(v)) return null
  return {
    five_hour: toQuotaWindow(pick(v, 'five_hour')),
    seven_day: toQuotaWindow(pick(v, 'seven_day')),
    fable: toQuotaWindow(pick(v, 'fable')),
    reset_credits: toResetCredits(pick(v, 'reset_credits')),
    limit_hit: toLimitHit(pick(v, 'limit_hit')),
    plan: optStr(pick(v, 'plan')),
    updated_at: str(pick(v, 'updated_at')),
    stale: bool(pick(v, 'stale'), false),
    host: str(pick(v, 'host'), hostOfQuotaKey(key ?? '')),
  }
}

export function toQuota(raw: unknown): QuotaMap {
  const root = isRec(raw) ? raw : {}
  const kinds = isRec(root.kinds) ? root.kinds : root
  const out: QuotaMap = {}
  for (const [k, v] of Object.entries(kinds)) out[k] = toKindQuota(v, k)
  return out
}

function toLabel(v: unknown): IssueLabel | null {
  if (typeof v === 'string') return v ? { name: v, color: null } : null
  if (!isRec(v)) return null
  const name = str(pick(v, 'name'))
  if (!name) return null
  const c = str(pick(v, 'color')).replace(/^#/, '')
  return { name, color: /^[0-9a-fA-F]{6}$/.test(c) ? c : null }
}

export function toIssue(v: unknown): Issue | null {
  if (!isRec(v)) return null
  const number = num(pick(v, 'number'), 0)
  if (!number) return null
  const state = str(pick(v, 'state'), 'open').toLowerCase()
  return {
    number,
    title: str(pick(v, 'title')),
    state: state === 'closed' ? 'closed' : 'open',
    labels: arr(pick(v, 'labels')).map(toLabel).filter((l): l is IssueLabel => l !== null),
    url: str(pick(v, 'url')),
    updated_at: str(pick(v, 'updated_at')),
    author: (() => {
      const a = pick(v, 'author')
      return isRec(a) ? str(pick(a, 'login')) : str(a)
    })(),
    body_excerpt: str(pick(v, 'body_excerpt')),
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
  return arr(pick(root, 'issues')).map(toIssue).filter((i): i is Issue => i !== null)
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

/** 不認得的一律 `unknown`，免得「可以砍嗎」變成猜的。 */
function toOwner(v: unknown): MemOwner {
  const s = str(v)
  return s === 'bot' || s === 'pane' || s === 'herdr' ? s : 'unknown'
}

/** SPEC §15.2 */
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

/** SPEC §15 */
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
    browsers: (Array.isArray(h.browsers) ? h.browsers : []).filter(isRec).map((b) => ({
      name: str(b.name),
      tabs: n(b.tabs),
      bytes: n(b.bytes),
      processes: n(b.processes),
    })),
    // 讀不出 → null，不要畫成「剩 0」
    machine: isRec(h.machine) && n(h.machine.total_bytes) > 0
      ? { total_bytes: n(h.machine.total_bytes), available_bytes: n(h.machine.available_bytes) }
      : null,
  }))
  return {
    total_bytes: n(r.total_bytes),
    herdr_bytes: n(r.herdr_bytes),
    agents_bytes: n(r.agents_bytes),
    processes: n(r.processes),
    hosts,
    projects: (Array.isArray(r.projects) ? r.projects : []).filter(isRec).map((pm) => ({
      project_id: str(pm.project_id),
      host: str(pm.host),
      panes: n(pm.panes),
      bytes: n(pm.bytes),
    })),
  }
}

// 群組任務（mission，docs/API.md「群組任務」）

const MISSION_DELIVERIES = ['push_main', 'pr'] as const
const MISSION_ON_5H = ['wait', 'switch'] as const
const MISSION_STATUSES = ['open', 'paused', 'done', 'cancelled'] as const
const MISSION_PHASES_SERVER = [
  'planning',
  'executing',
  'reviewing',
  'verifying',
  'waiting_quota',
  'awaiting_agm',
  'done',
  'cancelled',
  'paused',
] as const
const MISSION_ROLES = ['executor', 'reviewer', 'verifier'] as const
const MISSION_EVENT_KINDS = [
  'instruction',
  'report',
  'note',
  'verified',
  'round',
  'paused',
  'resumed',
  'cancelled',
  'delivered',
  'completed',
  'question',
  'answer',
] as const

export function toMission(v: unknown): Mission | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'mission_id'))
  if (!id) return null
  const completed = optStr(pick(v, 'completed_at'))
  const cancelled = optStr(pick(v, 'cancelled_at'))
  const paused = optStr(pick(v, 'paused_reason'))
  return {
    id,
    project_id: str(pick(v, 'project_id')),
    client_request_id: str(pick(v, 'client_request_id')),
    text: str(pick(v, 'text')),
    delivery_mode: oneOf<MissionDelivery>(pick(v, 'delivery_mode'), MISSION_DELIVERIES, 'pr'),
    executor_kind: oneOf<BotKind>(pick(v, 'executor_kind'), BOT_KINDS, 'claude'),
    on_5h_limit: oneOf<MissionOn5h>(pick(v, 'on_5h_limit'), MISSION_ON_5H, 'wait'),
    max_rounds: num(pick(v, 'max_rounds'), 2),
    rounds_used: num(pick(v, 'rounds_used'), 0),
    paused_reason: paused,
    paused_detail: optStr(pick(v, 'paused_detail')),
    result_summary: optStr(pick(v, 'result_summary')),
    parent_mission_id: optStr(pick(v, 'parent_mission_id')),
    // daemon 沒給才照同規則推
    status: oneOf<MissionStatus>(
      pick(v, 'status'),
      MISSION_STATUSES,
      cancelled ? 'cancelled' : completed ? 'done' : paused ? 'paused' : 'open',
    ),
    phase: (() => {
      const p = pick(v, 'phase')
      return typeof p === 'string' && (MISSION_PHASES_SERVER as readonly string[]).includes(p)
        ? (p as MissionPhaseServer)
        : null
    })(),
    created_at: str(pick(v, 'created_at')),
    updated_at: str(pick(v, 'updated_at'), str(pick(v, 'created_at'))),
    completed_at: completed,
    cancelled_at: cancelled,
  }
}

export function toMissionAssignment(v: unknown): MissionAssignment | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'assignment_id'))
  if (!id) return null
  const role = pick(v, 'role', 'mission_role')
  return {
    id,
    role:
      typeof role === 'string' && (MISSION_ROLES as readonly string[]).includes(role) ? (role as MissionRole) : null,
    status: str(pick(v, 'status')),
    target_bot_id: optStr(pick(v, 'target_bot_id', 'bot_id')),
    turn_status: optStr(pick(v, 'turn_status')),
    turn_error: optStr(pick(v, 'turn_error')),
    follow_up_of: optStr(pick(v, 'follow_up_of')),
    resume_at: optStr(pick(v, 'resume_at')),
    created_at: str(pick(v, 'created_at')),
    completed_at: optStr(pick(v, 'completed_at')),
  }
}

export function toMissions(raw: unknown): Mission[] {
  const root = isRec(raw) ? raw : {}
  const list = Array.isArray(raw) ? raw : arr(pick(root, 'missions', 'items'))
  const out: Mission[] = []
  for (const m of list) {
    const one = toMission(m)
    if (one) out.push(one)
  }
  return out
}

export function toMissionEvent(v: unknown, missionId = ''): MissionEvent | null {
  if (!isRec(v)) return null
  const id = str(pick(v, 'id', 'event_id'))
  if (!id) return null
  const raw = pick(v, 'payload', 'payload_json')
  let parsed: unknown = raw
  if (typeof raw === 'string') {
    try {
      parsed = JSON.parse(raw)
    } catch {
      parsed = { text: raw }
    }
  }
  return {
    id,
    reply_to: optStr(pick(v, 'reply_to')),
    mission_id: str(pick(v, 'mission_id'), missionId),
    kind: oneOf<MissionEventKind>(pick(v, 'kind'), MISSION_EVENT_KINDS, 'note'),
    text: str(pick(v, 'text')),
    relay_from: optStr(pick(v, 'relay_from')),
    payload: isRec(parsed) ? parsed : null,
    created_at: str(pick(v, 'created_at')),
  }
}

export function toMissionDetail(raw: unknown): MissionDetail | null {
  if (!isRec(raw)) return null
  const base = toMission(pick(raw, 'mission') ?? raw)
  if (!base) return null
  const events: MissionEvent[] = []
  for (const e of arr(pick(raw, 'events'))) {
    const one = toMissionEvent(e, base.id)
    if (one) events.push(one)
  }
  const assignments: MissionAssignment[] = []
  for (const a of arr(pick(raw, 'assignments'))) {
    const one = toMissionAssignment(a)
    if (one) assignments.push(one)
  }
  const revisions: MissionRevisionRef[] = []
  for (const r of arr(pick(raw, 'revisions'))) {
    if (!isRec(r)) continue
    const rid = str(pick(r, 'id'))
    if (!rid) continue
    revisions.push({
      id: rid,
      text: str(pick(r, 'text')),
      status: oneOf<MissionStatus>(pick(r, 'status'), MISSION_STATUSES, 'open'),
      created_at: str(pick(r, 'created_at')),
      result_summary: optStr(pick(r, 'result_summary')),
    })
  }
  const parentRaw = pick(raw, 'parent')
  let parent: MissionParentRef | null = null
  if (isRec(parentRaw)) {
    const pid = str(pick(parentRaw, 'id'))
    if (pid) {
      parent = {
        id: pid,
        text: optStr(pick(parentRaw, 'text')) ?? undefined,
        status: oneOf<MissionStatus>(pick(parentRaw, 'status'), MISSION_STATUSES, 'done'),
        result_summary: optStr(pick(parentRaw, 'result_summary')),
        missing: pick(parentRaw, 'missing') === true,
      }
    }
  }
  return { ...base, events: sortByTimeStable(events), assignments, revisions, parent }
}
