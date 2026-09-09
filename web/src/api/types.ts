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
   * SPEC-team §2.1：`user` = 使用者建立（受 TOML 投影管轄）；`team` = daemon 替某個 Team
   * 建立的臨時成員。舊 daemon 沒有這個欄位時一律當 `user`。
   */
  managed_by: BotManagedBy
  /** SPEC-team §2.1：team 成員身分；null = 一般 bot。 */
  team: BotTeamRef | null
  /** 由哪個 bot 的 agent 用 herdr 開出來的子 agent（名稱 `<父 agent 名>-<字尾>`）；null = 頂層。 */
  parent_bot_id: string | null
  /** SPEC-team §2.2：pane 的工作目錄；null = 用 `project.path`。 */
  cwd: string | null
  /**
   * 只存在於瀏覽器：分身 / 新增按下去的那一刻先放進清單的佔位列（灰的、不能點），
   * daemon 建好之後被真的那一列取代。`id` 是 `pending:` 開頭的假 id。
   */
  pending?: boolean
  /**
   * herdr 那邊的 agent 名稱（`GET /api/state` 的 `bots[].agent_name`）。有 active run 時是
   * 這個 run 實際用的名字，否則是「下次啟動會用的」。debug 時要拿它去 herdr 對照 pane。
   * 舊 daemon 沒有這個欄位 → null。
   */
  agent_name: string | null
  created_at: string
}

export type BotManagedBy = 'user' | 'team' | 'child'

export interface BotTeamRef {
  team_id: string
  role: TeamRole
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
  /** Effective Herdr session; old daemons may omit it and the normalizer returns null. */
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
  native_session_id: string | null
  transcript_path: string | null
  started_at: string
  ended_at: string | null
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
  /** SPEC-team §2.1：這則訊息屬於哪個 Team；null = 一般訊息。 */
  team_id: string | null
  /** SPEC-team §2.1：relay 的來源 bot_id（daemon 代轉時的說話者）；null = 使用者 / daemon 自己。 */
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
  /** SPEC-team §10.2：`projects[].teams[]` 攤平；舊 daemon 沒有這個欄位時為空陣列。 */
  teams: Team[]
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
  /** Pane geometry; null on an older daemon that does not report it. */
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

export type WsEventType =
  | 'bot_status'
  | 'message_added'
  | 'turn_updated'
  | 'project_changed'
  | 'bot_changed'
  | 'daemon_status'
  | 'host_changed'
  | 'turn_progress'
  | 'quota_updated'
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
  // `claude --model` 的 alias（含 Fable 5.1）。
  claude: ['opus', 'sonnet', 'haiku', 'fable'],
  codex: ['gpt-5.5', 'gpt-5.6-sol', 'gpt-5.6-luna', 'gpt-6-astra'],
  // `grok models`（grok 1.0.13，2026-09-06）：grok-4.6（預設）、grok-4.5
  grok: ['grok-4.6', 'grok-4.5'],
}

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
  /** 這台上的 Chromium 系瀏覽器（Chrome / ego）：分頁數與 RSS（2026-09-08）。舊 daemon 沒有 → 空陣列。 */
  browsers: BrowserMem[]
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

export interface KindQuota {
  five_hour: QuotaWindow | null
  seven_day: QuotaWindow | null
  /** Max 方案才有的 Fable 週額度（`Current week (Fable)`），跟 `seven_day` 同型；沒有就是 null。 */
  fable: QuotaWindow | null
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

// ------------------------------------------------------------ Issue Team（SPEC-team）

/** SPEC-team §8.1 team phase。終態：`done | aborted | failed`。 */
export type TeamPhase =
  | 'starting'
  | 'planning'
  | 'working'
  | 'finishing'
  | 'done'
  | 'paused'
  | 'aborting'
  | 'aborted'
  | 'failed'

export const TEAM_PHASES: readonly TeamPhase[] = [
  'starting',
  'planning',
  'working',
  'finishing',
  'done',
  'paused',
  'aborting',
  'aborted',
  'failed',
]

/** 終態（只剩 cleanup 可做）。 */
export const TEAM_TERMINAL_PHASES: readonly TeamPhase[] = ['done', 'aborted', 'failed']

export type TeamRole = 'pm' | 'worker' | 'reviewer'

/** SPEC-team §6.4 交付方式。**預設 `branch`**（使用者裁決；`pr` 會 push 到 origin）。 */
export type TeamDeliver = 'branch' | 'pr'

/** SPEC-team §8.2 task 狀態機。終態：`merged | skipped | failed`。 */
export type TeamTaskState =
  | 'queued'
  | 'working'
  | 'reported'
  | 'reviewing'
  | 'changes_requested'
  | 'exhausted'
  | 'blocked_by_worker'
  | 'rebasing'
  | 'merging'
  | 'merged'
  | 'skipped'
  | 'failed'

export const TEAM_TASK_STATES: readonly TeamTaskState[] = [
  'queued',
  'working',
  'reported',
  'reviewing',
  'changes_requested',
  'exhausted',
  'blocked_by_worker',
  'rebasing',
  'merging',
  'merged',
  'skipped',
  'failed',
]

/** 需要人介入的 task 狀態（看板「需要你」欄；`decide` 只在這些狀態有效）。 */
export const TEAM_TASK_NEEDS_USER: readonly TeamTaskState[] = ['exhausted', 'blocked_by_worker']

export type TeamEventKind = 'relay' | 'phase' | 'merge' | 'note' | 'user'
export type TeamEventStatus = 'pending' | 'delivered' | 'dropped'

/** SPEC-team §9.2 預算物件。使用者裁決的預設值。 */
export interface TeamBudget {
  max_relays: number
  max_review_rounds: number
  max_wall_clock_min: number
  quota_stop_pct: number
}

export const TEAM_BUDGET_DEFAULTS: TeamBudget = {
  max_relays: 40,
  max_review_rounds: 2,
  max_wall_clock_min: 120,
  quota_stop_pct: 90,
}

/**
 * SPEC-team §7.1：**併行數** `0`（無限）或 1–4，預設 1。
 *
 * 欄位名還是 `workers.count`（相容），但語意是「最多同時跑幾個 task」，不是「有幾個人」。
 */
export const TEAM_WORKERS_MAX = 4
export const TEAM_WORKERS_DEFAULT = 1

/**
 * SPEC-team §4.5（2026-09-09）：`count = 0` 是**無限**——佇列裡有幾個 issue 就同時做幾個，
 * 執行者數隨 PM 派工放大。是一個新的值，不是「沒有執行者」。
 */
export const TEAM_WORKERS_UNLIMITED = 0
/** 無限模式下同時進行的 issue 上限（daemon `MAX_CONCURRENT_ISSUES`）。 */
export const TEAM_MAX_CONCURRENT_ISSUES = 6
/** 無限模式下全隊執行者上限（daemon `MAX_TEAM_WORKERS`）。 */
export const TEAM_MAX_TEAM_WORKERS = 12

/** 建 team 時的額度預檢門檻（§9.2）：≥70% 已用 → 黃色警告。 */
export const TEAM_QUOTA_WARN_PCT = 70

/** SPEC-team §9.2 `usage_json`。 */
export interface TeamUsage {
  relays: number
  review_rounds_total: number
  elapsed_min: number
  per_bot: Record<string, { turns: number }>
}

export const TEAM_USAGE_EMPTY: TeamUsage = { relays: 0, review_rounds_total: 0, elapsed_min: 0, per_bot: {} }

export interface TeamMember {
  bot_id: string
  role: TeamRole
  /** cleanup soft-deletes the member bot but keeps it in the Team record. */
  deleted?: boolean
}

/** `tasks_summary`：以 task 狀態為 key 的計數（daemon 只保證 `total` 與有值的狀態）。 */
export type TeamTasksSummary = Partial<Record<TeamTaskState, number>> & { total: number }

/** SPEC-team §2.3：issue 佇列裡的一項。 */
export type TeamIssueState = 'queued' | 'working' | 'done' | 'failed' | 'skipped'

export interface TeamIssue {
  id: string
  seq: number
  issue_number: number
  issue_title: string
  issue_url: string
  state: TeamIssueState
  /** 開始處理後才有：這個 issue 自己的整合分支。 */
  branch: string | null
  summary: string | null
  pr_url: string | null
  issue_closed_at: string | null
  /** 佇列跳過它的原因（`merge_conflict` / `review_exhausted` / `pm_abort`）。 */
  fail_reason: string | null
  started_at: string | null
  ended_at: string | null
}

export interface TeamIssuesSummary {
  total: number
  done: number
  failed: number
  queued: number
}

export const TEAM_ISSUE_STATES: readonly TeamIssueState[] = ['queued', 'working', 'done', 'failed', 'skipped']

export const TEAM_ISSUE_STATE_LABEL: Record<TeamIssueState, string> = {
  queued: '待處理',
  working: '進行中',
  done: '已交付',
  failed: '失敗',
  skipped: '略過',
}

/** SPEC-team §4.5：`quota_low` 暫停時撞到上限的那一個成員與那一個額度視窗。 */
export interface TeamPauseQuotaMember {
  bot_id: string
  /** 完整 bot 名（`ttxka1d-i2-rev`）。 */
  name: string
  /** 面板上寫的短名（`rev`、`dev-1`）。 */
  short: string
  role: TeamRole | null
  kind: string
  /** 用哪個身分登入的（`cc2`）；null = 這個 kind 只有一個帳號。 */
  identity: string | null
  host: string
  window: 'five_hour' | 'seven_day'
  used_pct: number
  remaining_pct: number
  resets_at: string | null
}

/** SPEC-team §4.5 / §10.2：`pause_reason` 的結構化細節。目前只有 `quota_low` 會帶。 */
export interface TeamPauseDetail {
  /** 當時的 `budget.quota_stop_pct`。 */
  stop_pct: number
  /** 全部不夠的成員，最嚴重的在前面。 */
  members: TeamPauseQuotaMember[]
}

/** SPEC-team §10.2 `GET /api/state` 的 team 物件。 */
export interface Team {
  /** §2.3：整個 issue 佇列。下面的 `issue_*` 是「當前這一項」的鏡像。 */
  issues: TeamIssue[]
  current_issue_id: string | null
  issues_summary: TeamIssuesSummary
  id: string
  project_id: string
  issue_number: number
  issue_title: string
  issue_url: string
  phase: TeamPhase
  /** `paused` 的機器碼原因（例：`budget_relays`、`member_lost:dev-1`、`gate:merge`）。 */
  pause_reason: string | null
  /** SPEC-team §4.5：機器碼補不完的那一半——`quota_low` 是誰的額度不夠。舊 daemon 沒有 → null。 */
  pause_detail: TeamPauseDetail | null
  branch: string
  deliver: TeamDeliver
  supervised: boolean
  members: TeamMember[]
  tasks_summary: TeamTasksSummary
  budget: TeamBudget
  usage: TeamUsage
  pr_url: string | null
  /** SPEC-team §10.7：使用者從這個 team 關掉 issue 的時間；null = 沒關過（daemon 不會自己關）。 */
  issue_closed_at: string | null
  /** 這個 team 處理的是專案哪個 submodule 的 issue；`''` = 專案本身。 */
  repo: string
  created_at: string
  started_at: string | null
  ended_at: string | null
}

/** SPEC-team §10.3 `GET /api/teams/:id` 的一件 task。 */
export interface TeamTask {
  id: string
  /** §2.3：這個 task 屬於佇列裡的哪一個 issue。 */
  issue_id: string | null
  seq: number
  title: string
  brief: string
  files: string[]
  /** §4.5：`null` = 還在排隊，沒有執行者在做。 */
  worker_bot_id: string | null
  branch: string
  state: TeamTaskState
  round: number
  last_report: string | null
  last_verdict: string | null
  merge_sha: string | null
  updated_at: string
}

/** SPEC-team §10.3 `GET /api/teams/:id` = state 的 team 物件 + 這些。 */
export interface TeamDetail extends Team {
  tasks: TeamTask[]
  summary: string | null
  base_ref: string
  base_sha: string
  worktree_root: string
  /** `GET /teams/:id` 的 `roles`：三個角色各自是用什麼設定建出來的。 */
  roles: TeamRoles
}

/** SPEC-team §10.4 `GET /api/teams/:id/events` 的一則。 */
export interface TeamEvent {
  id: string
  kind: TeamEventKind
  from_bot_id: string | null
  to_bot_id: string | null
  task_id: string | null
  turn_id: string | null
  status: TeamEventStatus | null
  payload: Record<string, unknown>
  created_at: string
}

/** SPEC-team §10.1 一張角色卡的設定。 */
export interface TeamRoleSpec {
  kind: BotKind
  model: string | null
  effort: string | null
  fast: boolean
  identity: string | null
  persona_extra: string
}

export interface TeamWorkersSpec extends TeamRoleSpec {
  /** `TEAM_WORKERS_UNLIMITED`（0，無限）或 1–`TEAM_WORKERS_MAX` */
  count: number
}

/** `POST /api/projects/:id/teams` 的 body。 */
export interface NewTeamInput {
  /** §2.3：依序處理的 issue 佇列。 */
  issue_numbers: number[]
  /** 要解的是哪個 submodule 的 issue（相對路徑）；省略 = 專案本身。 */
  repo?: string
  pm: TeamRoleSpec
  workers: TeamWorkersSpec
  /** null = 不審查，`reported` 直接進整合。 */
  reviewer: TeamRoleSpec | null
  base: string
  deliver: TeamDeliver
  supervised: boolean
  budget: TeamBudget
}

/** `PATCH /api/teams/:id`。只送有變更的欄位。 */
export interface PatchTeamInput {
  budget?: Partial<TeamBudget>
  supervised?: boolean
  deliver?: TeamDeliver
  /** SPEC-team §10.5：三個角色同一個形狀，各自獨立送。 */
  workers?: TeamRolePatch
  pm?: TeamRolePatch
  reviewer?: TeamRolePatch
}

/**
 * 一個角色可改的欄位。省略 = 不動，`null` = 清成該 kind 的預設。
 *
 * `kind` 是特別的：成員跑哪個 CLI 是開 pane 時決定的，所以改 kind 一定是**換一個 bot**
 * （SPEC-team §7.6），`apply` 管不到它；該成員正在跑一個 turn 時 daemon 回 409 `member busy`。
 */
export interface TeamRolePatch {
  kind?: BotKind
  model?: string | null
  effort?: string | null
  fast?: boolean
  identity?: string | null
  apply?: 'next' | 'now'
  /** 只有 `workers` 有：併行數，`TEAM_WORKERS_UNLIMITED`（0）或 1–`TEAM_WORKERS_MAX`。 */
  count?: number
}

/** `roles_json.<role>`：那個角色是用什麼設定建出來的（`GET /teams/:id` 的 `roles`）。 */
export interface TeamWorkerSpec {
  kind: BotKind
  model: string | null
  effort: string | null
  fast: boolean
  identity: string | null
}

/** 三個角色的 spec；`null` = 這隊沒有這個角色（例如建立時沒選 reviewer）。 */
export interface TeamRoles {
  pm: TeamWorkerSpec | null
  workers: TeamWorkerSpec | null
  reviewer: TeamWorkerSpec | null
  /**
   * 併行數（`roles_json.workers.count`）：`0` = 無限。`null` = 舊 daemon 沒送，
   * 呼叫端退回「數現有的執行者」。
   */
  workers_count: number | null
}

/** `PatchTeamInput` 的 key，也是 daemon `roles_json` 的 key。 */
export type TeamRoleKey = 'pm' | 'workers' | 'reviewer'

export const TEAM_ROLE_KEYS: readonly TeamRoleKey[] = ['pm', 'workers', 'reviewer']

export const TEAM_ROLE_KEY_LABEL: Record<TeamRoleKey, string> = {
  pm: 'PM',
  workers: '執行者',
  reviewer: 'Reviewer',
}

/** 側欄成員列的 `bot.team.role` → `roles_json` 的 key。 */
export function roleKeyOfMember(role: TeamRole): TeamRoleKey {
  return role === 'worker' ? 'workers' : role
}

/** SPEC-team §10.7 `POST /api/teams/:id/close-issue` 的回應。 */
export interface TeamIssueClosed {
  number: number
  url: string
  /** 這個 issue 在呼叫之前就已經是 closed（別人先關的，或 PR 關掉的）。 */
  already_closed: boolean
}

export type TeamControlAction = 'pause' | 'resume' | 'approve' | 'abort' | 'cleanup'
export type TeamTaskDecision = 'rework' | 'force_merge' | 'skip'
/**
 * SPEC-team §6.5a：`DELETE /teams/:id?branches=` 的處置。
 * `keep`（預設）留下整合分支與所有 task 分支；`delete` 才 `git branch -D`
 * ——唯一會銷毀工作成果的路徑，UI 必須二次確認。遠端分支一律不動。
 */
export type TeamBranchDisposal = 'keep' | 'delete'

export const TEAM_PHASE_LABEL: Record<TeamPhase, string> = {
  starting: '啟動中',
  planning: '規劃中',
  working: '進行中',
  finishing: '收尾中',
  done: '已完成',
  paused: '已暫停',
  aborting: '中止中',
  aborted: '已中止',
  failed: '失敗',
}

/** phase chip 的視覺分級（樣式 class 尾綴）。 */
export function teamPhaseTone(phase: TeamPhase): 'busy' | 'run' | 'warn' | 'ok' | 'dim' {
  switch (phase) {
    case 'starting':
      return 'busy'
    case 'planning':
    case 'working':
    case 'finishing':
      return 'run'
    case 'paused':
      return 'warn'
    case 'done':
      return 'ok'
    default:
      return 'dim'
  }
}

export const TEAM_ROLE_LABEL: Record<TeamRole, string> = {
  pm: 'PM',
  worker: '執行者',
  reviewer: 'Reviewer',
}

export const TEAM_TASK_STATE_LABEL: Record<TeamTaskState, string> = {
  queued: '待派',
  working: '進行中',
  reported: '已回報',
  reviewing: '審查中',
  changes_requested: '待修正',
  exhausted: '審查回合用盡',
  blocked_by_worker: '執行者卡住',
  rebasing: 'rebase 中',
  merging: '整合中',
  merged: '已合併',
  skipped: '已跳過',
  failed: '失敗',
}

/** SPEC-team §4.5 / §7.5 / §8.1 的 `pause_reason` 機器碼 → 中文。 */
const TEAM_PAUSE_LABEL: Record<string, string> = {
  user: '你按了暫停',
  budget_relays: '轉送次數用完',
  budget_time: '時間預算用完',
  quota_low: '額度過低',
  review_exhausted: '審查回合用盡',
  pm_repeat: 'PM 重複派同一件工作',
  pm_abort: 'PM 認為做不下去',
  ask_user: 'PM 有問題要問你',
  protocol_error: 'am-team 區塊解析失敗',
  member_failed: '成員啟動失敗',
  member_lost: '成員已離線',
  member_blocked: '成員等待終端回應',
  delivery_unknown: '上一則轉送的送達狀態未知',
  merge_conflict: '合併衝突無法自動解決',
  integration_dirty: '整合分支的工作樹不乾淨',
  worktree_missing: 'worktree 目錄不見了',
  deliver_failed: '交付失敗（push / gh）',
  upstream: 'herdr / DB 錯誤',
  'gate:dispatch': '等待放行：派工',
  'gate:merge': '等待放行：合併',
  'gate:deliver': '等待放行：交付',
  // 不是暫停原因，是 `done → starting` 這則 phase 事件的 reason（SPEC-team §2.5.2）；
  // 時間軸用同一張表翻譯 reason，少了它會印出英文代碼。
  reopen: '使用者追加 issue',
}

/**
 * SPEC-team §7.3：`ttxka1d-i2-dev-1` → `dev-1`, `ttxka1d-pm` → `pm`, `i42-rev` → `rev`.
 *
 * Team 成員的名字是 `<team slug>-[i<issue>-]<role>`；同一個畫面上五個成員的 slug 與 issue
 * 號完全一樣，擺在彼此旁邊時純粹是雜訊。完整名字留在 tooltip 裡。
 */
export function teamShortName(botName: string): string {
  // 只有在剝掉前綴之後「還讀得出角色」時才剝，否則已經很短的 `dev-1` 會被縮成 `1`。
  const role = /^(pm|rev|reviewer|dev|worker)(-|$)/i
  let out = botName
  for (let i = 0; i < 2 && !role.test(out); i += 1) {
    const peeled = out.replace(/^[a-z0-9]+-/i, '')
    if (peeled === out) break
    out = peeled
  }
  return role.test(out) ? out : botName
}

/** `member_lost:dev-1` → 「成員已離線（dev-1）」；未知碼原樣顯示。 */
export function teamPauseLabel(reason: string | null): string {
  if (!reason) return ''
  if (TEAM_PAUSE_LABEL[reason]) return TEAM_PAUSE_LABEL[reason]
  const i = reason.indexOf(':')
  if (i > 0) {
    const head = reason.slice(0, i)
    const detail = reason.slice(i + 1)
    if (TEAM_PAUSE_LABEL[head]) return `${TEAM_PAUSE_LABEL[head]}（${detail}）`
  }
  return reason
}
