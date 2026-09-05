/**
 * Wire types for the agents-manager daemon API.
 *
 * Source of truth: docs/SPEC.md §7 (REST + WebSocket), appendix C (SQLite schema), and the
 * daemon's own `daemon/src/api.rs` handlers, which these were checked against:
 *
 *   GET  /api/session          -> {token, port}
 *   GET  /api/state            -> {daemon_seq, connected, herdr_session, hosts:[...],
 *                                  projects:[{id,path,label,workspace_id,host,
 *                                             bots:[{...,args,autostart,inject_hooks,run,lamp,unread}]}]}
 *   POST /api/hosts            -> {name, connected, error?}       (SPEC §11.6)
 *   DELETE /api/hosts/:name    -> {}
 *   POST /api/hosts/:name/reconnect -> {name, connected, error?}
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

export type BotKind = 'claude' | 'codex' | 'grok'
/** 全部支援的 kind（下拉選單與 normalize 共用；SPEC §12 新增 `grok`）。 */
export const BOT_KINDS: readonly BotKind[] = ['claude', 'codex', 'grok']
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
  /** SPEC §11.6: `"local"` = 本機，其餘為 `hosts[].name` */
  host: string
  created_at: string
}

/** SPEC §11.2 / §11.6 — 遠端主機（透過 SSH 轉發的遠端 herdr）。 */
export interface Host {
  /** `[a-z][a-z0-9_-]{0,31}`；`"local"` 保留給本機，不會出現在這個清單 */
  name: string
  /** ssh 目標，例如 `m4p@100.112.229.82`，可為 ssh_config 別名 */
  ssh: string
  ssh_port: number
  herdr_session: string
  remote_path: string
  hook_port: number
  connected: boolean
  /** 連線失敗原因（ssh / herdr），連線正常時為 null */
  error: string | null
}

/**
 * 本機的保留 host id。`GET /api/state` 的 `hosts[]` 第一筆一定是它（ssh 等欄位為 null），
 * 但 `normalize.toState()` 會把它濾掉：store 的 `hosts` 只含遠端主機，本機狀態看 `connected`。
 */
export const LOCAL_HOST = 'local'

export interface NewHostInput {
  name: string
  ssh: string
  ssh_port?: number
  herdr_session?: string
  remote_path?: string
  hook_port?: number
  /** 額外的 ssh 參數，原樣附加到每個 ssh 指令（例：`["-i","~/.ssh/id_x"]`） */
  ssh_opts?: string[]
}

/** `POST /api/hosts` / `POST /api/hosts/:name/reconnect` 的回應。 */
export interface HostResult {
  name: string
  connected: boolean
  error: string | null
}

/** §11.2 進階欄位的預設值（表單預填，與 daemon 的預設一致）。 */
export const HOST_DEFAULTS = {
  ssh_port: 22,
  herdr_session: 'agents-manager',
  remote_path: '/opt/homebrew/bin:$HOME/.local/bin',
  hook_port: 7788,
} as const

export interface Bot {
  id: string
  project_id: string
  name: string
  kind: BotKind
  /**
   * 模型別名（API.md v3.3）。`null` = 不指定，由 agent CLI 自己決定預設。
   * daemon 會把它翻成 `--model <值>`（claude）/ `-m <值>`（codex、grok）。
   */
  model: string | null
  args: string[]
  autostart: boolean
  /** daemon extension: false = no hook injection (terminal-fallback path) */
  inject_hooks: boolean
  auto_approve: boolean
  /** 身份預設（`identities[].name`），null = 無 */
  identity: string | null
  /** 額外注入 pane 的環境變數（覆蓋 identity.env） */
  env: Record<string, string>
  created_at: string
}

/**
 * 身份預設（例如 `cc1` = 用另一個 `CLAUDE_CONFIG_DIR` 跑不同帳號）。
 * daemon 啟動 bot 時把 `env` 注入 pane、`args` 接在 daemon 注入參數之後。
 */
export interface Identity {
  name: string
  kind: BotKind
  env: Record<string, string>
  args: string[]
}

export interface NewIdentityInput {
  name: string
  kind: BotKind
  env: Record<string, string>
  /** 契約保留；UI 的新增身份表單不再提供 args（身份的本體是 env）。 */
  args?: string[]
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
  /**
   * SPEC §13：同一次群組發言（`POST /projects/:id/chat`）產生的訊息共用一個 group_id
   * （每個收件 bot 的 user 副本、以及「未送達」的 system 註記）；其他訊息為 null。
   */
  group_id: string | null
  created_at: string
}

/** SPEC §13.4 `GET /api/projects/:id/messages` 的一則：訊息 + 它屬於哪個 bot。 */
export interface GroupMessage extends Message {
  bot_id: string
  bot_name: string
}

export interface GroupMessagesPage {
  project_id: string
  messages: GroupMessage[]
  has_more: boolean
}

/** SPEC §13.4 `POST /api/projects/:id/chat` 的回應。 */
export interface GroupChatResult {
  group_id: string
  sent: { bot_id: string; bot_name: string; turn_id: string; message_id: string | null; delivery: TurnDelivery }[]
  skipped: { bot_id: string; bot_name: string; reason: GroupSkipReason; detail: string }[]
}

/** §13.3 略過原因（機器碼）；人類可讀說明在 `detail` 與該 bot 對話裡的 system 訊息。 */
export type GroupSkipReason =
  | 'not_running'
  | 'blocked'
  | 'in_flight'
  | 'unknown_delivery'
  | 'conflict'
  | 'not_found'
  | 'bad_request'
  | 'upstream'

export const GROUP_SKIP_LABEL: Record<GroupSkipReason, string> = {
  not_running: '未啟動',
  blocked: '等待終端回應',
  in_flight: '上一回合進行中',
  unknown_delivery: '上一回合送達狀態未知',
  conflict: '狀態衝突',
  not_found: '找不到 bot',
  bad_request: '請求不合法',
  upstream: 'herdr / DB 錯誤',
}

/** Normalized `GET /api/state`. */
export interface AppState {
  daemon_seq: number
  connected: boolean
  hosts: Host[]
  identities: Identity[]
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
  | 'host_changed'
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
  /** SPEC §11.6：省略或 `"local"` = 本機 */
  host?: string
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
  /** `null` / 省略 = 不帶 `--model`（由 CLI 自己決定） */
  model?: string | null
  /**
   * 契約保留，但 UI 不再提供 args 欄位（使用者決定）：新增 Bot 時一律省略，
   * 由 daemon 用預設值。要調參數請改身份（identity）或 config.toml。
   */
  args?: string[]
  autostart: boolean
  auto_approve?: boolean
  identity?: string | null
  env?: Record<string, string>
}

/**
 * `PATCH /api/bots/:id`（API.md v3.3）。只送有變更的欄位；
 * `model` / `identity` 傳 `null` 代表清除。
 */
export interface PatchBotInput {
  name?: string
  model?: string | null
  args?: string[]
  autostart?: boolean
  auto_approve?: boolean
  inject_hooks?: boolean
  identity?: string | null
  env?: Record<string, string>
}

/**
 * `PATCH /api/bots/:id` 的回應。`needs_restart = true` 代表設定已寫入 config，
 * 但目前這個 Run 仍跑在舊參數上，要 `POST /api/bots/:id/restart` 才會生效。
 */
export interface PatchBotResult {
  needs_restart: boolean
}

/**
 * 各 kind 的常用模型別名（下拉選單用；使用者仍可用「自訂…」輸入任意字串）。
 * 空字串 = `（預設）`，送出時轉成 `null`。
 */
export const MODEL_OPTIONS: Record<BotKind, readonly string[]> = {
  claude: ['opus', 'sonnet', 'haiku'],
  codex: ['gpt-5.5', 'gpt-5.6-luna', 'gpt-6-astra'],
  // `grok models`（grok 1.0.13，2026-09-06）：grok-4.6（預設）、grok-4.5
  grok: ['grok-4.6', 'grok-4.5'],
}

/** 「（預設）」與「自訂…」在 `<select>` 裡的 sentinel 值。 */
export const MODEL_DEFAULT = ''
export const MODEL_CUSTOM = ' custom'
