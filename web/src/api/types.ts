/**
 * Wire types for the agents-manager daemon API.
 *
 * Source of truth: docs/SPEC.md §7 (REST + WebSocket), appendix C (SQLite schema), and the
 * daemon's own `daemon/src/api.rs` handlers, which these were checked against:
 *
 *   GET  /api/session          -> {token, port}
 *   GET  /api/state            -> {daemon_seq, connected, herdr_session,
 *                                  projects:[{id,path,label,workspace_id,
 *                                             bots:[{...,args,autostart,inject_hooks,run,lamp,unread}]}]}
 *   POST /api/projects         -> {project_id}
 *   POST /api/projects/:id/bots-> {bot_id}
 *   POST /api/bots/:id/start   -> {run_id}
 *   POST /api/bots/:id/prompt  -> {turn_id, message_id, delivery}
 *   GET  /api/bots/:id/messages-> {bot_id, conversation_id, messages[], turns[], has_more}
 *   GET  /api/bots/:id/terminal-> {bot_id, run_id, pane_id, source, text, revision, truncated, agent_status}
 *   errors                     -> {error, reason?|message?|what?, ...extra}
 *
 * `normalize.ts` is the single place that absorbs any remaining shape drift.
 */

export type BotKind = 'claude' | 'codex'
export type RunState = 'starting' | 'running' | 'stopping' | 'stopped' | 'exited'
export type AgentStatus = 'idle' | 'working' | 'blocked' | 'unknown'
export type TurnStatus = 'in_flight' | 'completed' | 'completed_fallback' | 'failed'
export type TurnDelivery = 'pending' | 'ok' | 'unknown' | 'failed'
export type TurnOrigin = 'web' | 'external'
export type MessageRole = 'user' | 'assistant' | 'system'
export type MessageSource = 'web' | 'hook' | 'transcript' | 'terminal_fallback' | 'system'
export type TerminalSource = 'visible' | 'recent_unwrapped'

export interface Project {
  id: string
  path: string
  label: string
  workspace_id: string | null
  created_at: string
}

export interface Bot {
  id: string
  project_id: string
  name: string
  kind: BotKind
  args: string[]
  autostart: boolean
  /** daemon extension: false = no hook injection (terminal-fallback path) */
  inject_hooks: boolean
  created_at: string
}

/**
 * Composite status lamp (SPEC §2.2). The daemon also computes this as `bots[].lamp`
 * in `GET /api/state`, but the UI recomputes it from the run so that `bot_status`
 * WebSocket updates (which carry only the run) stay authoritative.
 */
export type Lamp = 'disconnected' | 'offline' | 'starting' | 'stopping' | 'idle' | 'working' | 'blocked' | 'unknown'

export interface Run {
  id: string
  bot_id: string
  state: RunState
  agent_status: AgentStatus
  workspace_id: string | null
  pane_id: string | null
  adopted: boolean
  native_session_id: string | null
  transcript_path: string | null
  started_at: string
  ended_at: string | null
}

export interface Turn {
  id: string
  conversation_id: string
  run_id: string | null
  bot_id?: string | null
  origin: TurnOrigin
  status: TurnStatus
  delivery: TurnDelivery
  client_request_id: string | null
  created_at: string
  completed_at: string | null
}

export interface Message {
  id: string
  conversation_id: string
  turn_id: string | null
  bot_id?: string | null
  role: MessageRole
  content: string
  source: MessageSource
  incomplete: boolean
  created_at: string
}

/** Normalized `GET /api/state`. */
export interface AppState {
  daemon_seq: number
  connected: boolean
  projects: Project[]
  bots: Bot[]
  /** active runs, keyed by bot_id downstream */
  runs: Run[]
  turns: Turn[]
}

export interface TerminalSnapshot {
  text: string
  revision: number | null
  truncated: boolean
  source: TerminalSource
}

export interface PromptResult {
  turn_id: string
  message_id: string | null
  delivery: TurnDelivery
}

/** `GET /api/bots/:id/messages` */
export interface MessagesPage {
  bot_id: string
  conversation_id: string
  messages: Message[]
  turns: Turn[]
  has_more: boolean
}

export interface StartResult {
  run_id: string
}

/** Envelope of `/ws` frames (daemon `WsEvent`: `{seq, type, data}`). */
export interface WsFrame {
  seq?: number
  type: string
  data?: unknown
}

export type WsEventType =
  | 'bot_status'
  | 'message_added'
  | 'turn_updated'
  | 'project_changed'
  | 'bot_changed'
  | 'daemon_status'
  | 'resync'

export interface ApiErrorBody {
  /** machine code: `conflict` | `bad_request` | `not_found` | `upstream` */
  error?: string
  reason?: string
  message?: string
  what?: string
  turn_id?: string
  run_id?: string
  [k: string]: unknown
}

export class ApiError extends Error {
  status: number
  body: ApiErrorBody
  constructor(status: number, body: ApiErrorBody, fallback: string) {
    // The daemon puts the human-readable text in `reason` (conflicts), `message`
    // (bad request / upstream) or `what` (not found).
    super(String(body.reason ?? body.message ?? body.what ?? body.error ?? fallback))
    this.name = 'ApiError'
    this.status = status
    this.body = body
  }
}

/** Payloads for the config-mutating endpoints. */
export interface NewProjectInput {
  path: string
  label: string
}

export interface DirEntry {
  name: string
  path: string
  git: boolean
}

export interface DirListing {
  path: string
  parent: string | null
  home: string
  entries: DirEntry[]
}

export interface NewBotInput {
  name: string
  kind: BotKind
  args: string[]
  autostart: boolean
}
