/**
 * Wire types for the agents-manager daemon API.
 *
 * Source of truth: docs/SPEC.md §7 (REST + WebSocket), appendix C (SQLite schema), and the
 * daemon's own `daemon/src/api.rs` handlers, which these were checked against:
 *
 *   GET  /api/session          -> {token, port}
 *   GET  /api/state            -> {daemon_seq, connected, default_connected, herdr_session, hosts:[...],
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
export type TurnStatus = 'queued' | 'in_flight' | 'completed' | 'completed_fallback' | 'failed'
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
  /** v4.0：目錄的 GitHub remote（daemon 由 `git remote` / `gh` 解析），null = 非 GitHub 專案 */
  github: ProjectGithub | null
  created_at: string
}

export interface ProjectGithub {
  owner: string
  repo: string
  url: string
}

/** `GET /api/projects/:id/submodules` 的一項：專案裡的 git submodule 與它自己的 GitHub origin。 */
export interface ProjectSubmodule {
  /** 相對於專案根目錄的路徑（`.gitmodules` 的 `path`）。 */
  path: string
  github: ProjectGithub | null
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
  connected: boolean
  /** 連線失敗原因（ssh / herdr），連線正常時為 null */
  error: string | null
  /**
   * v4.0：在本機終端 attach 同一個 herdr session 的指令，例如
   * `herdr --remote m4p@100.112.229.82 --session agents-manager`（本機為 `herdr --session agents-manager`）。
   */
  attach_command: string
  /** v4.0：該主機上各 agent CLI 的偵測結果。 */
  tools: ToolMap
  /**
   * v4.0：每個 `[[identities]]` 在「這台主機上」的登入狀態（daemon 的 `hosts[].identities`）。
   * 身份是全域設定，但它指到的帳號是不是能用因主機而異，所以這是 per-host 的。
   */
  identity_status: IdentityStatusMap
}

/** v4.0 `hosts[].tools.<kind>`。`logged_in` 為 null = 未知（例如 CLI 沒有可查的登入狀態）。 */
export interface ToolStatus {
  installed: boolean
  path: string | null
  version: string | null
  logged_in: boolean | null
}

export type ToolMap = Record<BotKind, ToolStatus>

/** 沒有資訊時的預設（視為已安裝、登入未知，避免誤報缺工具）。 */
export const TOOL_UNKNOWN: ToolStatus = { installed: true, path: null, version: null, logged_in: null }

/**
 * v4.0 `hosts[].identities.<name>`：某個身份在該主機上的登入狀態。
 * `logged_in` 為 null = 問不到（CLI 沒裝、偵測失敗），不等於未登入。
 */
export interface IdentityStatus {
  name: string
  kind: BotKind
  logged_in: boolean | null
  /** `logged_in: null` 的可見原因（CLI 缺少、指令失敗、輸出無法解析等）。 */
  reason: string | null
  /** 登入的是誰（claude 是 e-mail、codex 是 `ChatGPT`、grok 是 `grok.com`）。 */
  account: string | null
  /** claude 的 `subscriptionType`（`max` / `team` …）。 */
  plan: string | null
  /**
   * `config` = config.toml 的 `[[identities]]`（可編輯、可刪）；
   * `shell` = daemon 從這台主機登入 shell 的 `ccN` alias 認出來的（唯讀，SPEC §16）。
   */
  source: 'config' | 'shell'
  /** 這台主機上這個身份指到的設定目錄（`cc0` 這種預設帳號沒有）。顯示用。 */
  config_dir: string | null
}

/** key = 身份名稱。缺的身份代表這台主機還沒偵測過。 */
export type IdentityStatusMap = Record<string, IdentityStatus>

/**
 * 本機的保留 host id。`GET /api/state` 的 `hosts[]` 第一筆一定是它（ssh 等欄位為 null），
 * 但 `normalize.toState()` 會把它濾掉：store 的 `hosts` 只含遠端主機，本機狀態看 `connected`。
 */
export const LOCAL_HOST = 'local'

/**
 * 額度 map 的 key（SPEC §14）：本機是裸的 `claude` / `claude:cc1`，遠端主機加前綴
 * （`m4p/claude`）。daemon 用同一條規則組 key。
 */
export function quotaKey(host: string, base: string): string {
  return !host || host === LOCAL_HOST ? base : `${host}/${base}`
}

/** `m4p/claude:cc1` → `m4p`；裸 key（本機）→ `local`。 */
export function hostOfQuotaKey(key: string): string {
  const i = key.indexOf('/')
  return i > 0 ? key.slice(0, i) : LOCAL_HOST
}

export interface NewHostInput {
  name: string
  ssh: string
  ssh_port?: number
  herdr_session?: string
  remote_path?: string
  /** 額外的 ssh 參數，原樣附加到每個 ssh 指令（例：`["-i","~/.ssh/id_x"]`） */
  ssh_opts?: string[]
}

/** `POST /api/hosts` / `POST /api/hosts/:name/reconnect` 的回應。 */
export interface HostResult {
  name: string
  connected: boolean
  error: string | null
}

/** `GET|POST /api/hosts/:name/gh` — 該主機上 GitHub CLI 能不能用。 */
export interface GhAccount {
  login: string
  active: boolean
  ok: boolean
}

export interface GhPending {
  user_code: string
  verification_uri: string
  verification_uri_complete: string | null
  expires_in: number
}

export type GhLoginMode = 'auto' | 'copy' | 'device' | 'switch'

export interface GhStatus {
  name: string
  installed: boolean
  path: string | null
  logged_in: boolean
  account: string | null
  accounts: GhAccount[]
  mode: string | null
  pending: GhPending | null
  error: string | null
}

/** §11.2 進階欄位的預設值（表單預填，與 daemon 的預設一致）。 */
export const HOST_DEFAULTS = {
  ssh_port: 22,
  herdr_session: 'agents-manager',
  remote_path: '/opt/homebrew/bin:$HOME/.local/bin',
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
  /** reasoning effort（grok：low | medium | high | xhigh；codex：依 `GET /api/models` 的 `efforts`）；null = CLI 預設 */
  effort: string | null
  /** v4.0：codex 的 fast / priority service tier（`--fast`）。 */
  fast: boolean
  /** v4.0：人設（system prompt 前置文字），null = 無。 */
  persona: string | null
  args: string[]
  autostart: boolean
  /** daemon extension: false = no hook injection (terminal-fallback path) */
  inject_hooks: boolean
  auto_approve: boolean
  /** 身份預設（`identities[].name`），null = 無 */
  identity: string | null
  /** 額外注入 pane 的環境變數（覆蓋 identity.env） */
  env: Record<string, string>
  /** Herdr session override; `default` means this bot was discovered in the user's session. */
  herdr_session: string | null
  /**
   * `user` = 使用者建立（受 TOML 投影管轄）；`child` = 別的 bot 用 herdr 開出來的子 agent。
   */
  managed_by: BotManagedBy
  /** 由哪個 bot 的 agent 用 herdr 開出來的子 agent（名稱 `<父 agent 名>-<字尾>`）；null = 頂層。 */
  parent_bot_id: string | null
  /**
   * 使用者釘的「主要執行的 bot」（`PATCH /api/bots/:id {primary}`）。純顯示用的釘選：
   * 不影響啟動參數、不用重啟，存在 daemon 所以手機與電腦看到同一組。
   */
  primary: boolean
  /** pane 的工作目錄；null = 用 `project.path`。 */
  cwd: string | null
  /**
   * 只存在於瀏覽器：分身 / 新增按下去的那一刻先放進清單的佔位列（灰的、不能點），
   * daemon 建好之後被真的那一列取代。`id` 是 `pending:` 開頭的假 id。
   */
  pending?: boolean
  /**
   * herdr 那邊的 agent 名稱（`GET /api/state` 的 `bots[].agent_name`）。有 active run 時是
   * 這個 run 實際用的名字，否則是「下次啟動會用的」。debug 時要拿它去 herdr 對照 pane。
   */
  agent_name: string | null
  created_at: string
}

export type BotManagedBy = 'user' | 'child'

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
  /** Effective Herdr session; null when the run has none recorded. */
  herdr_session: string | null
  /** agent 目前替自己取的名字（herdr `terminal_title_stripped`，claude 會寫成當前任務摘要）。 */
  agent_title: string | null
  /** pane 底下那條狀態列的原文（claude 自訂 statusLine 的輸出，已去 ANSI）。 */
  status_line: string | null
  /** statusLine 的完整原始資料——網頁版顯示的就是它，不受終端寬度壓縮。 */
  status: StatusInfo | null
  /**
   * claude 已經把新版下載好、等重啟才會換過去時的那句話
   * （`Update installed · Restart to update`）。沒有更新在等就是 null。
   */
  update_notice: string | null
  /**
   * 上一回合被 API 斷線截斷時，pane 上那行原文
   * （`API Error: Connection lost mid-response. The response above may be incomplete.`）。
   * hook 與 herdr 都會把這種回合報成 done，所以「看起來做完了」不代表真的做完——
   * daemon 把那行掛在 run 上（`runs.turn_error`），null = 上一回合是正常收尾的。
   * 下一回合一開始就會被清掉。
   */
  turn_error: string | null
  /**
   * SPEC §4.4a：這個 run **實際上**在跑的模型／強度／fast，daemon 從真正送出去的 argv 讀回來
   * （codex 的模型與強度只在啟動時吃得到，`bots` 那份是「下次啟動會用的設定」）。
   * `null` = 不知道（daemon 沒有親手啟動它，例如被收編的 pane），這時不做任何比對。
   */
  runtime_model: string | null
  runtime_effort: string | null
  runtime_fast: boolean | null
  native_session_id: string | null
  transcript_path: string | null
  started_at: string
  ended_at: string | null
}

/** 批次重啟裡被跳過的那一顆與原因（`POST /api/bots/restart-idle`，SPEC §6.9）。 */
export interface RestartSkip {
  bot_id: string
  name: string
  /** 機器判讀用：`working` / `blocked` / `turn_in_flight` / `not_running` / `unknown_status`。 */
  reason: string
  /** 給人看的那句（daemon 寫好的，前端不另編一套）。 */
  reason_label: string
}

/** `POST /api/bots/restart-idle` 立刻回來的計畫；實際進度走 WS。 */
export interface RestartPlan {
  batch_id: string
  total: number
  planned: { bot_id: string; name: string }[]
  skipped: RestartSkip[]
}

/** 前端自己維護的批次狀態（計畫 + 一路收到的 WS 進度）。 */
export interface RestartBatch {
  id: string
  total: number
  /** 已經跑完的顆數（成功或失敗都算）。 */
  done: number
  /** 正在重啟的那顆名字；null = 還沒開始或已經結束。 */
  current: string | null
  ok: string[]
  failed: { name: string; error: string }[]
  skipped: RestartSkip[]
  finished: boolean
}

/**
 * claude statusLine 傳進來的完整資訊（`daemon/src/statusline_cmd.rs`）。
 *
 * 使用者的 statusLine 腳本為了塞進終端一行會縮寫（email 只取前 5 碼、模型縮成 `OP5`），
 * 網頁沒有這個限制，所以直接用原始欄位。
 */
export interface StatusInfo {
  /** claude 登入的帳號（daemon 依該主機的身份登入狀態回報）。 */
  account_email: string | null
  /** 身份在該主機沒登入、實際跑的是預設帳號時的警告。 */
  account_warning: string | null
  model_name: string | null
  model_id: string | null
  effort: string | null
  thinking: boolean
  fast_mode: boolean
  /** context window：已用百分比、已用 token、總容量。 */
  context_used_pct: number | null
  context_used_tokens: number | null
  context_size: number | null
  five_hour_pct: number | null
  /** epoch 秒 */
  five_hour_resets_at: number | null
  seven_day_pct: number | null
  seven_day_resets_at: number | null
  cost_usd: number | null
  cwd: string | null
  version: string | null
  session_name: string | null
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

/** One image sent with a user message (`daemon/src/attach.rs`). */
export interface Attachment {
  id: string
  name: string
  mime: string
  size: number
  /** Absolute path on the bot's host — what the agent was told to read. */
  path: string
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
  /** 隨這則訊息拖放進來的圖片；沒有附件時為空陣列。 */
  attachments: Attachment[]
  /** relay 的來源 bot_id（daemon 代轉時的說話者）；null = 使用者 / daemon 自己。 */
  relay_from: string | null
  /**
   * `terminal_fallback` 訊息當下的**整個 pane 畫面**。`content` 是從裡面裁出來的那段，
   * 裁錯時（例如剛好抓到「Running 1 shell command…」）真正的回覆還在這裡面。
   * 其他來源的訊息是 null。
   */
  terminal_snapshot: string | null
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
  /** The observed user's Herdr default session; separate from the manager session. */
  default_connected: boolean
  /** v4.0：本機的 attach 指令（`hosts[0]`，reserved `local` 的 `attach_command`）。 */
  attach_command: string
  /** v4.0：本機的工具偵測（`hosts[0].tools`）。 */
  tools: ToolMap
  /** v4.0：本機的身份登入偵測（`hosts[0].identities`）。 */
  identity_status: IdentityStatusMap
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
  pane_id: string | null
  /** Pane geometry; null when herdr does not report it. */
  columns: number | null
  rows: number | null
}

/**
 * `POST /api/hosts/:name/shells` — 一個 daemon 幫我們在某台主機開的純 shell pane
 * （沒有 agent、沒有 run）。`pane_id` 是後續每一支端點的把手，只在 daemon 這一輪有效。
 */
export interface HostShell {
  host: string
  pane_id: string
  tab_id: string
  workspace_id: string
  /** herdr 實際開起來的目錄，不一定等於要求的那個。 */
  cwd: string
  created_at: string
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

export interface PatchProjectInput {
  label?: string
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
  /** 名稱撞到時讓 daemon 自己往後找 `<base>-<n>`（快速新增用）；回應會帶實際用的 `name`。 */
  name_auto?: boolean
  kind: BotKind
  /** `null` / 省略 = 不帶 `--model`（由 CLI 自己決定） */
  effort?: string | null
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
  /** v4.0：codex fast tier */
  fast?: boolean
  /** v4.0：人設 */
  persona?: string | null
}

/**
 * `PATCH /api/bots/:id`（API.md v3.3）。只送有變更的欄位；
 * `model` / `identity` 傳 `null` 代表清除。
 */
export interface PatchBotInput {
  name?: string
  model?: string | null
  effort?: string | null
  args?: string[]
  autostart?: boolean
  auto_approve?: boolean
  inject_hooks?: boolean
  identity?: string | null
  env?: Record<string, string>
  fast?: boolean
  persona?: string | null
  /** 釘成「主要執行的 bot」。純顯示，`needs_restart` 一定是 false。 */
  primary?: boolean
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
/** grok 的靜態 effort（`GET /api/models` 失敗時退回；實際清單以模型為準，grok-4.6 含 xhigh）。 */
export const EFFORT_OPTIONS = ['low', 'medium', 'high', 'xhigh'] as const
/**
 * claude 的 `--effort`（2.1+；`claude --help`：low, medium, high, xhigh, max）。
 * TUI 的 `/effort` 是拉桿（←/→ 調整、Enter 確認），沒有帶參數的形式，所以改了要重啟才生效。
 */
export const CLAUDE_EFFORT_OPTIONS = ['low', 'medium', 'high', 'xhigh', 'max'] as const
/** codex 的靜態 effort（API.md §12.2；`GET /api/models` 失敗時退回）。 */
export const CODEX_EFFORT_OPTIONS = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'] as const

/**
 * 強度按鈕／狀態列顯示用的字——**不翻譯**，就是原廠自己的字（`claude --help` / codex
 * `model/list` / grok CLI 都是這樣拼的），只把首字母大寫成標題的樣子（`high` → `High`）。
 * 用中文詞（低/中/高）會跟原廠文件、CLI 畫面（`● high · /effort`）對不起來，使用者查資料
 * 或截圖對照時反而多一層翻譯要核對。
 */
export function effortLabel(level: string): string {
  return level ? level.charAt(0).toUpperCase() + level.slice(1) : level
}

export const MODEL_OPTIONS: Record<BotKind, readonly string[]> = {
  // `claude --model` 的 alias（含 Fable 5.1）。由輕到重排（2026-09-09 使用者決定）：
  // 選單的順序就是這一組的正式順序，`sortModels` 拿它排 API 回來的清單。
  claude: ['haiku', 'sonnet', 'opus', 'fable'],
  // gpt-5.5 拿掉了（2026-09-09）：上一代的模型還在帳號清單裡，但選單不該把它擺在等同的位置。
  // 順序同 claude，由輕到重（2026-09-09 使用者決定）。
  codex: ['gpt-5.6-luna', 'gpt-5.6-terra', 'gpt-5.6-sol', 'gpt-6-astra'],
  // `grok models`（grok 1.0.13，2026-09-06）：grok-4.6（預設）、grok-4.5
  grok: ['grok-4.6', 'grok-4.5'],
}

/**
 * 不放進選單的模型 id（2026-09-09）。
 *
 * codex 的 `model/list` 還會回上一代的 `gpt-5.5`，但選單把它跟現行的幾顆並排，等於在邀請人
 * 選一個沒有理由再選的模型。清單是 CLI 給的，daemon 照抄（`/api/models` 要忠實反映 CLI），
 * 所以過濾發生在**選單這一層**：真的要用的人走「自訂…」；已經設成它的 bot 那顆按鈕照樣留著，
 * 不然畫面會靜靜地把它顯示成「自訂」。
 */
export const HIDDEN_MODELS: readonly string[] = ['gpt-5.5']

/** 「（預設）」與「自訂…」在 `<select>` 裡的 sentinel 值。 */
export const MODEL_DEFAULT = ''
export const MODEL_CUSTOM = 'custom'

// ------------------------------------------------------------------ v4.0

/** `GET /api/models?kind=&host=` 的一筆。 */
export interface ModelInfo {
  id: string
  display_name: string
  description: string
  is_default: boolean
  /** 該模型的預設 reasoning effort（null = 不適用） */
  default_effort: string | null
  /** 可選的 effort（空陣列 = 該模型不支援） */
  efforts: string[]
  /** 含 `priority` 時 UI 才顯示「Fast」開關 */
  service_tiers: { id: string; name: string; description: string }[]
}

/** 「Fast」開關對應的 service tier id。 */
export const FAST_TIER = 'priority'

/** `GET /api/quota` 的一個視窗（5 小時 / 7 天）。 */
export interface QuotaWindow {
  used_pct: number
  /** ISO 時間；daemon 可能給 null */
  resets_at: string | null
  /**
   * daemon 算好的門檻旗標（`LOW_REMAINING_PCT` = 30）——額度條要不要把剩餘數字顯示出來，
   * 由 API server 決定，UI 不可自己用 `used_pct` 寫死比較。
   */
  low: boolean
  /** daemon 算好的門檻旗標（`CRITICAL_REMAINING_PCT` = 5）——bot 側欄列要不要提示。 */
  critical: boolean
}

/** `GET /api/mem` / WS `mem_updated`（SPEC §15）：herdr 進程樹的常駐記憶體。 */
export interface HostMem {
  host: string
  /** herdr 本身 */
  herdr_bytes: number
  /** 它底下的 pane / agent CLI */
  agents_bytes: number
  total_bytes: number
  processes: number
  /** 這台量不到時的原因；此時數字都是 0，不是「真的 0」。 */
  error: string | null
  /** 這台上的 Chromium 系瀏覽器（Chrome / ego）：分頁數與 RSS（2026-09-08）；沒有就是空陣列。 */
  browsers: BrowserMem[]
  /**
   * 整台機器的總量與剩餘可用量（2026-09-12）。`total_bytes` 只講「herdr 樹吃掉多少」，回答不了
   * 「還能不能再開一顆 bot」；daemon 讀不出時為 null，UI 就只顯示已用量。
   */
  machine: MachineMem | null
}

/** 整台機器的記憶體（`HostMem.machine`，SPEC §15）。 */
export interface MachineMem {
  total_bytes: number
  /** 現在還可用的量。Linux 取 MemAvailable；macOS 取 free+inactive+speculative+purgeable。 */
  available_bytes: number
}

export interface BrowserMem {
  name: string
  /** renderer process 數 ≈ 分頁數。 */
  tabs: number
  bytes: number
  processes: number
}

export interface MemSnapshot {
  total_bytes: number
  herdr_bytes: number
  agents_bytes: number
  processes: number
  hosts: HostMem[]
}

/** `GET /api/mem/processes` 的一列（SPEC §15.2）：那個 RAM 數字裡的一個程序。 */
export interface MemProcess {
  pid: number
  ppid: number
  rss_bytes: number
  /** 執行檔名，路徑已剝掉：`claude` / `codex` / `zsh`。 */
  exe: string
  argv: string
  /** herdr 注入的 pane（`w168:p1`）；讀不到環境時 null。 */
  pane_id: string | null
  /** 那個 pane 所屬 herdr session 的 socket（pane id 是 per-session 的）；讀不到時 null。 */
  socket_path: string | null
  /** AG Man 起的 bot 才有；bot 已刪也還在，此時 `bot_name` 為 null。 */
  bot_id: string | null
  bot_name: string | null
  project_id: string | null
  owner: MemOwner
  /** 自己 ＋ 所有子孫的 RSS，也就是「砍掉能省多少」。清單依它降冪。 */
  subtree_bytes: number
  children: number
}

/** `bot` 要走「停止 bot」；`pane` / `unknown` 才可以直接送訊號。 */
export type MemOwner = 'bot' | 'pane' | 'herdr' | 'unknown'

export interface MemProcesses {
  host: string
  sampled_at: string
  processes: MemProcess[]
}

/** `GET /api/search/messages` 的一列：這個 bot 的對話裡命中幾次，以及最新一次的前後文。 */
export interface MessageHit {
  hits: number
  snippet: string
}

/**
 * Codex 的「額度重置券」（`rateLimitResetCredits`）。額度歸零時 OpenAI 會送一張可以立刻
 * 把桶子清掉的券（codex TUI 上的 `Reset usage`）；桶子說的是「什麼時候自己回血」，這個說的是
 * 「你現在就能清掉，還有幾張」。只有 codex 有，其餘 kind 一律 null。
 */
export interface QuotaResetCredits {
  /** 現在可用的張數。 */
  available: number
  /** 第一張可用券的名稱，例如 `Full reset (Weekly + 5 hr)`。 */
  title: string | null
  /** 那張券的到期時間（ISO）；券會過期。 */
  expires_at: string | null
}

export interface KindQuota {
  five_hour: QuotaWindow | null
  seven_day: QuotaWindow | null
  /** Max 方案才有的 Fable 週額度（`Current week (Fable)`），跟 `seven_day` 同型；沒有就是 null。 */
  fable: QuotaWindow | null
  /** codex 的額度重置券；沒有（或不是 codex）就是 null。 */
  reset_credits: QuotaResetCredits | null
  /**
   * CLI 自己印的「這個帳號被擋住了」橫幅。5h／7d 是速率視窗，codex 的 credits 用完時它們可以
   * 是滿的（2026-09-12：量表全滿、送出卻一直回 hit your usage limit），所以這格要單獨看。
   */
  limit_hit: QuotaLimitHit | null
  plan: string | null
  updated_at: string
  /** 這份額度是在哪台主機讀到的（`local` 或 `hosts[].name`）。 */
  host: string
}

/**
 * `GET /api/quota` → `{kinds: {...}}`。key 為 kind（`claude` / `codex` / `grok`）或
 * `<kind>:<identity>`（例如 `claude:cc1`）；null = 該 kind 沒有額度資訊。
 *
 * 額度是**按主機**分開的（SPEC §14）：本機用裸 key，遠端主機在前面加自己的名字
 * （`m4p/claude`、`m4p/claude:cc1`）。header 的額度列一次只看一台主機——目前檢視的
 * bot／專案所在的那台——所以遠端 bot 的 statusline 不會蓋到本機那列。
 */
export type QuotaMap = Record<string, KindQuota | null>

/** CLI 印出來的上限橫幅（`KindQuota.limit_hit`）。 */
export interface QuotaLimitHit {
  /** 橫幅原文。 */
  message: string
  /** 橫幅寫的恢復時間（ISO）；沒寫就是 null，那要等下一回合跑成功才會消失。 */
  until: string | null
  /** 什麼時候撞到的（ISO）。 */
  at: string
}

/** `POST /api/hosts/:name/tools/install {kind, via_bot_id}` → `{turn_id}`。 */
export interface InstallToolResult {
  turn_id: string
}

/** `GET /api/projects/:id/issues` 的一筆（列表不含完整 body）。 */
export interface Issue {
  number: number
  title: string
  state: 'open' | 'closed'
  labels: IssueLabel[]
  url: string
  updated_at: string
  author: string
  body_excerpt: string
}

export interface IssueLabel {
  name: string
  /** 6 碼 hex（無 #），null = 未知 */
  color: string | null
}

/** `GET /api/projects/:id/issues/:number` — 含完整 `body`。 */
export interface IssueDetail extends Issue {
  body: string
}

// ---------------------------------------------- 群組任務（mission，docs/API.md「群組任務」）

/** 交付方式（D2）：直接推 main，或開 PR 給使用者看。 */
export type MissionDelivery = 'push_main' | 'pr'

/** 5h 撞限時（D5）：原地等重置，或直接換下一個身分。7d 撞限一律換。 */
export type MissionOn5h = 'wait' | 'switch'

/** 從欄位算出來的，不是另外存一份：`cancelled_at → done_at → paused_reason → open`。 */
export type MissionStatus = 'open' | 'paused' | 'done' | 'cancelled'

export type MissionRole = 'executor' | 'reviewer' | 'verifier'

export type MissionEventKind =
  | 'instruction'
  | 'report'
  | 'note'
  | 'verified'
  | 'round'
  | 'paused'
  | 'resumed'
  | 'cancelled'
  | 'delivered'
  | 'completed'
  /** 使用者對成果追問（已完成的任務也能問）。 */
  | 'question'
  /** 回覆：使用者回答暫停，或 AGM 回答某一則 `question`（`reply_to` 指回去）。 */
  | 'answer'

export interface Mission {
  id: string
  project_id: string
  client_request_id: string
  text: string
  delivery_mode: MissionDelivery
  executor_kind: BotKind
  on_5h_limit: MissionOn5h
  max_rounds: number
  rounds_used: number
  /** `max_rounds` / `no_fable_for_verifier` / `push_main_failed` / `pr_failed`，或呼叫端自己寫的。 */
  paused_reason: string | null
  paused_detail: string | null
  result_summary: string | null
  /** 非 null ＝ 這筆是某個成果的續作（追加修改）。 */
  parent_mission_id: string | null
  status: MissionStatus
  /**
   * P1b：daemon 從 assignments 推導的細分階段。`null` = 舊 daemon 沒給，前端自己從事件推。
   *
   * `done | cancelled | paused` 同 `status`；其餘看最新一件還開著的交辦：`executing` /
   * `reviewing` / `verifying`（依 role），那件停在 `quota_blocked` 時是 `waiting_quota`；
   * 還沒有任何交辦＝`planning`；交辦都結案了但任務還開著＝`awaiting_agm`。
   */
  phase: MissionPhaseServer | null
  created_at: string
  updated_at: string
  completed_at: string | null
  cancelled_at: string | null
}

/** daemon 給的細分階段（P1b）。 */
export type MissionPhaseServer =
  | 'planning'
  | 'executing'
  | 'reviewing'
  | 'verifying'
  | 'waiting_quota'
  | 'awaiting_agm'
  | 'done'
  | 'cancelled'
  | 'paused'

/** `relay_from`：`null` = 使用者本人，bot id = 那顆 bot，`"daemon"` = daemon 自己記的。 */
export interface MissionEvent {
  id: string
  mission_id: string
  kind: MissionEventKind
  text: string
  relay_from: string | null
  payload: Record<string, unknown> | null
  /** `answer` 指回它回答的那則 `question` 的事件 id；其他事件是 null。 */
  reply_to: string | null
  created_at: string
}

/** 成果卡上串起來的一問一答。`answer` 為 null ＝ 還在等 AGM 回。 */
export interface MissionQna {
  question: MissionEvent
  answer: MissionEvent | null
}

/**
 * 這個任務的一件交辦（P1b）。`role` 與 `turn_error` 就是任務卡上「誰在跑」「為什麼換手」
 * 的權威來源——比從事件 payload 猜可靠。
 */
export interface MissionAssignment {
  id: string
  role: MissionRole | null
  status: string
  target_bot_id: string | null
  turn_status: string | null
  /** 撞限時 daemon 從 `run.turn_error` 抄過來的那一句。 */
  turn_error: string | null
  /** 這件是誰的 follow-up（撞限換手接手的那一件）。 */
  follow_up_of: string | null
  created_at: string
  completed_at: string | null
}

export interface MissionDetail extends Mission {
  events: MissionEvent[]
  /** 舊的在前。舊 daemon 沒有這個欄位就是空陣列。 */
  assignments: MissionAssignment[]
  /** 這筆成果的續作（新的在前）。舊 daemon 沒給就是空陣列。 */
  revisions: MissionRevisionRef[]
  /** 這筆是從哪個成果續作來的。`missing` ＝ 來源任務已經不在了。 */
  parent: MissionParentRef | null
}

export interface MissionRevisionRef {
  id: string
  text: string
  status: MissionStatus
  created_at: string
  result_summary: string | null
}

export interface MissionParentRef {
  id: string
  text?: string
  status?: MissionStatus
  result_summary?: string | null
  missing?: boolean
}

/** `POST /api/projects/:id/missions` 的 body（§11 的三個選項）。 */
export interface NewMissionInput {
  text: string
  client_request_id: string
  delivery_mode: MissionDelivery
  executor_kind: BotKind
  on_5h_limit: MissionOn5h
  max_rounds?: number
}
