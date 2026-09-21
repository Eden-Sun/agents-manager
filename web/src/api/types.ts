/** Wire types for the daemon API. Source of truth: SPEC §7, appendix C, `daemon/src/api.rs`, docs/API.md; shape drift is absorbed in `normalize.ts`. */

export type BotKind = 'claude' | 'codex' | 'grok'
/** SPEC §12 */
export const BOT_KINDS: readonly BotKind[] = ['claude', 'codex', 'grok']

/** claude 2.1.277+ 內建 `agents-md` plugin 的 `instructionFiles`（API.md §10）：這顆 claude bot 讀哪份專案指示檔。順序 = 面板上的順序。 */
export const INSTRUCTION_FILES = ['claude-md', 'claude-md-or-agents-md', 'claude-md-and-agents-md', 'managed-only'] as const
export type InstructionFiles = (typeof INSTRUCTION_FILES)[number]
/** daemon 沒設時釘的值（跟 2.1.277 以前一樣只讀 CLAUDE.md）。 */
export const INSTRUCTION_FILES_DEFAULT: InstructionFiles = 'claude-md'
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
  /** null = 非 GitHub 專案 */
  github: ProjectGithub | null
  created_at: string
}

export interface ProjectGithub {
  owner: string
  repo: string
  url: string
}

/** `GET /api/projects/:id/submodules` 的一項。 */
export interface ProjectSubmodule {
  /** 相對於專案根目錄 */
  path: string
  github: ProjectGithub | null
}

/** 每台主機的 herdr 版本（SPEC §11.6）。任一欄讀不到＝null，不猜。 */
export interface HerdrVersion {
  /** ping 回報的 server 版本；主機沒連上時為 null */
  server_version: string | null
  protocol: number | null
  /** protocol 是否在 daemon 實測過的清單；null＝不知道 */
  protocol_supported: boolean | null
  /** `herdr --version`（CLI）；server 與 CLI 可能不同版 */
  cli_version: string | null
  /** CLI 與 server 版本都知道而且不同 */
  mismatch: boolean
}

export const UNKNOWN_HERDR: HerdrVersion = { server_version: null, protocol: null, protocol_supported: null, cli_version: null, mismatch: false }

/** SPEC §11.2 / §11.6 — 遠端主機（透過 SSH 轉發的遠端 herdr）。 */
export interface Host {
  /** `[a-z][a-z0-9_-]{0,31}`；`"local"` 保留給本機，不會出現在這個清單 */
  name: string
  /** 可為 ssh_config 別名 */
  ssh: string
  ssh_port: number
  herdr_session: string
  remote_path: string
  connected: boolean
  /** 連線正常時為 null */
  error: string | null
  /** 在本機終端 attach 同一個 herdr session 的指令 */
  attach_command: string
  herdr: HerdrVersion
  tools: ToolMap
  /** daemon 的 `hosts[].identities`；per-host，因為同一身份的帳號能不能用因主機而異。 */
  identity_status: IdentityStatusMap
}

/** `logged_in` 為 null = 未知（例如 CLI 沒有可查的登入狀態）。 */
export interface ToolStatus {
  installed: boolean
  path: string | null
  version: string | null
  logged_in: boolean | null
}

export type ToolMap = Record<BotKind, ToolStatus>

/** 沒有資訊時的預設（視為已安裝、登入未知，避免誤報缺工具）。 */
export const TOOL_UNKNOWN: ToolStatus = { installed: true, path: null, version: null, logged_in: null }

/** `logged_in` 為 null = 問不到（CLI 沒裝、偵測失敗），不等於未登入。 */
export interface IdentityStatus {
  name: string
  kind: BotKind
  logged_in: boolean | null
  /** `logged_in: null` 的原因 */
  reason: string | null
  /** claude 是 e-mail、codex 是 `ChatGPT`、grok 是 `grok.com` */
  account: string | null
  /** claude 的 `subscriptionType` */
  plan: string | null
  /** `config` = config.toml（可編輯）；`shell` = 從主機 shell 的 `ccN` alias 認出來的（唯讀，SPEC §16）。 */
  source: 'config' | 'shell'
  /** `cc0` 這種預設帳號沒有 */
  config_dir: string | null
}

/** key = 身份名稱。缺的身份代表這台主機還沒偵測過。 */
export type IdentityStatusMap = Record<string, IdentityStatus>

/** `hosts[]` 第一筆一定是它，但 `normalize.toState()` 會濾掉：store 的 `hosts` 只含遠端，本機看 `connected`。 */
export const LOCAL_HOST = 'local'

/** SPEC §14：本機是裸 key（`claude:cc1`），遠端加前綴（`m4p/claude`）；須與 daemon 同規則。 */
export function quotaKey(host: string, base: string): string {
  return !host || host === LOCAL_HOST ? base : `${host}/${base}`
}

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
  /** 原樣附加到每個 ssh 指令 */
  ssh_opts?: string[]
}

export interface HostResult {
  name: string
  connected: boolean
  error: string | null
}

/** issue #104：開發者專用外部 Cargo verification worker。密碼永遠不從 API 回傳。 */
export interface RemoteCargoSettings {
  enabled: boolean
  host: string
  user: string
  ssh_port: number
  remote_root: string
  cargo_jobs: number
  password_set: boolean
}

export interface RemoteCargoInput {
  enabled: boolean
  host: string
  user: string
  ssh_port: number
  remote_root: string
  cargo_jobs: number
  /** undefined = 保留既有密碼；空字串 = 清除改用 key/agent auth。 */
  password?: string
}

/** `GET|POST /api/hosts/:name/gh` */
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

/** §11.2 表單預填，須與 daemon 預設一致。 */
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
  /** null = 不指定，由 CLI 決定預設（API.md v3.3） */
  model: string | null
  /** null = CLI 預設；codex 可選值見 `GET /api/models` 的 `efforts` */
  effort: string | null
  /** codex 的 priority service tier（`--fast`） */
  fast: boolean
  /** system prompt 前置文字，null = 無 */
  persona: string | null
  /** claude 才有：這顆 bot 現在讀哪份專案指示檔（沒設＝`claude-md`）；codex／grok 或舊 daemon 沒有這欄＝null。 */
  instruction_files: InstructionFiles | null
  args: string[]
  autostart: boolean
  /** false = no hook injection (terminal-fallback path) */
  inject_hooks: boolean
  auto_approve: boolean
  /** `identities[].name`，null = 無 */
  identity: string | null
  /** 覆蓋 identity.env */
  env: Record<string, string>
  /** `default` means this bot was discovered in the user's session. */
  herdr_session: string | null
  /** `user` = 使用者建立（受 TOML 投影管轄）；`child` = 別的 bot 用 herdr 開出來的子 agent。 */
  managed_by: BotManagedBy
  /** null = 頂層 */
  parent_bot_id: string | null
  /** 使用者釘的主要 bot：純顯示、不用重啟，存在 daemon 讓手機與電腦同步。 */
  primary: boolean
  /** 主力（★）那列的固定順序（issue #344，daemon 存 DB）；小的在前，沒拖過都是 0，舊 daemon 沒這欄＝0。 */
  primary_position: number
  /** 執行中的 CLI 載入的啟動設定跟現在存的不同＝要重啟（#353，daemon 從資料算，PATCH 回應掉了也看得到）。 */
  needs_restart: boolean
  /** null = 用 `project.path` */
  cwd: string | null
  /** 預覽模式（issue #253）：`/api/state` 帶的簡版；舊 daemon 沒有＝undefined，null＝沒開過。 */
  preview?: { status: 'off' | 'starting' | 'running' | 'failed'; port: number | null } | null
  /** 只存在於瀏覽器的佔位列（`id` 以 `pending:` 開頭），daemon 建好後被取代。 */
  pending?: boolean
  /** 有 active run 時是實際用的名字，否則是下次啟動會用的；debug 時拿去 herdr 對照 pane。 */
  agent_name: string | null
  /** daemon 算的未讀回合數（跨裝置共用，2026-09-15）；舊 daemon 沒有這欄就是 undefined。 */
  unread?: number
  /** daemon 存的已讀位置（跨裝置共用）；null＝還沒有。 */
  read_mark?: { at: string; id: string } | null
  created_at: string
}

export type BotManagedBy = 'user' | 'child'

/** 身份預設（例如 `cc1` = 另一個 `CLAUDE_CONFIG_DIR`）；`args` 接在 daemon 注入參數之後。 */
export interface Identity {
  name: string
  /** 哪一台主機的身分（SPEC §16.2）。`null`／缺 = 只適用本機。同名的 `cc1` 在不同機器上是不同帳號。 */
  host?: string | null
  kind: BotKind
  env: Record<string, string>
  args: string[]
}

export interface NewIdentityInput {
  name: string
  kind: BotKind
  env: Record<string, string>
  /** 契約保留；UI 不提供 */
  args?: string[]
}

/** SPEC §2.2. UI recomputes it from the run (not `bots[].lamp`) so run-only `bot_status` WS updates stay authoritative. */
export type Lamp = 'disconnected' | 'offline' | 'starting' | 'stopping' | 'idle' | 'working' | 'blocked' | 'unknown'

export interface Run {
  id: string
  bot_id: string
  state: RunState
  agent_status: AgentStatus
  workspace_id: string | null
  pane_id: string | null
  adopted: boolean
  herdr_session: string | null
  /** herdr `terminal_title_stripped`（claude 會寫成當前任務摘要） */
  agent_title: string | null
  /** statusLine 輸出原文，已去 ANSI */
  status_line: string | null
  /** statusLine 原始資料，網頁不受終端寬度壓縮 */
  status: StatusInfo | null
  /** `Update installed · Restart to update` 那句；null = 沒有更新在等 */
  update_notice: string | null
  /**
   * 上一回合被 API 斷線截斷的那行原文；hook 與 herdr 都會報成 done，所以要看這格。
   * null = 正常收尾；下一回合開始時清掉。
   */
  turn_error: string | null
  /** SPEC §4.4a：從實際 argv 讀回的模型／強度／fast；`null` = 不知道（例如被收編的 pane），不做比對。 */
  runtime_model: string | null
  runtime_effort: string | null
  runtime_fast: boolean | null
  /** issue #238：啟動時使用的身份；空字串＝本機預設帳號，`null`＝不知道（收編 pane／舊列）。 */
  runtime_identity: string | null
  native_session_id: string | null
  transcript_path: string | null
  started_at: string
  ended_at: string | null
  /** `agent_status` 最後一次真的改變的時間（daemon 觀察到的，不是這頁看到的）；issue #93。 */
  agent_status_since: string | null
}

/** `POST /api/bots/restart-idle`，SPEC §6.9。 */
export interface RestartSkip {
  bot_id: string
  name: string
  /** `working` / `blocked` / `turn_in_flight` / `not_running` / `unknown_status` / `no_longer_pending`（輪到它時狀態變了）/ `state_unreadable`（輪到它時 DB 讀不到它的狀態，這次沒動它） */
  reason: string
  /** daemon 寫好的，前端不另編一套 */
  reason_label: string
}

/** 立刻回來的計畫；實際進度走 WS。 */
export interface RestartPlan {
  batch_id: string
  total: number
  planned: { bot_id: string; name: string }[]
  skipped: RestartSkip[]
  /** 已經有一批在跑：`batch_id` 是那一批，其餘欄位是空的，進度看那一批的事件。 */
  already_running: boolean
}

/** 前端維護：計畫 + WS 進度。 */
export interface RestartBatch {
  id: string
  total: number
  /** 成功或失敗都算 */
  done: number
  /** null = 還沒開始或已經結束 */
  current: string | null
  ok: string[]
  failed: { name: string; error: string }[]
  skipped: RestartSkip[]
  finished: boolean
}

/** `daemon/src/statusline_cmd.rs`；用原始欄位，因為使用者的 statusLine 腳本會縮寫（email 前 5 碼、`OP5`）。 */
export interface StatusInfo {
  account_email: string | null
  /** 身份在該主機沒登入、實際跑的是預設帳號時的警告 */
  account_warning: string | null
  model_name: string | null
  model_id: string | null
  effort: string | null
  thinking: boolean
  fast_mode: boolean
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
  /** 沒有無損證據可以確認送達（grok／遠端／codex 還沒回報 session 的打字，以及 herdr `agent.prompt` 那條路）。
   *  `turns.delivery_verified = 0`。這**不代表**不會自動重送——那是 `autoResend` 的事（SPEC §4.4a）。 */
  unverified: boolean
  /** daemon 會不會在畫面證明它沒進去時自動重送這一則（`turns.auto_resend`）。舊 daemon 沒有這一欄 → 當作 true。 */
  autoResend: boolean
  /** issue #122：送出時 bot 沒在跑，daemon 先收下再啟動它（`turns.awaits_start`）。只對 `queued` 有意義。 */
  awaitsStart: boolean
  /** 上一次替它啟動 bot 失敗的原因（`turns.start_error`）；`null`＝沒失敗過。 */
  startError: string | null
  client_request_id: string | null
  created_at: string
  completed_at: string | null
}

/** `daemon/src/attach.rs` */
export interface Attachment {
  id: string
  name: string
  mime: string
  size: number
  /** Absolute path on the bot's host */
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
  /** SPEC §13：同一次群組發言的訊息共用；其他訊息為 null。 */
  group_id: string | null
  attachments: Attachment[]
  /** null = 使用者 / daemon 自己 */
  relay_from: string | null
  /** `terminal_fallback` 當下的整個 pane 畫面；`content` 裁錯時真正的回覆還在這裡。其他來源為 null。 */
  terminal_snapshot: string | null
  created_at: string
}

/** SPEC §13.4 `GET /api/projects/:id/messages` */
export interface GroupMessage extends Message {
  bot_id: string
  bot_name: string
}

export interface GroupMessagesPage {
  project_id: string
  messages: GroupMessage[]
  has_more: boolean
}

/** SPEC §13.4 `POST /api/projects/:id/chat` */
export interface GroupChatResult {
  group_id: string
  /** false＝一顆都沒送到（全被跳過）；舊 daemon 沒有這欄，用 `sent.length` 判斷（見 `groupSendDelivered`）。 */
  delivered?: boolean
  sent: { bot_id: string; bot_name: string; turn_id: string; message_id: string | null; delivery: TurnDelivery }[]
  skipped: { bot_id: string; bot_name: string; reason: GroupSkipReason; detail: string }[]
}

/** §13.3；人類可讀說明在 `detail`。 */
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

export interface AppState {
  daemon_seq: number
  connected: boolean
  /** The user's Herdr default session; separate from the manager session. */
  default_connected: boolean
  /** attach_command / tools / identity_status 取自本機 `hosts[0]`。 */
  attach_command: string
  /** 本機 `hosts[0].herdr` */
  herdr: HerdrVersion
  tools: ToolMap
  identity_status: IdentityStatusMap
  hosts: Host[]
  identities: Identity[]
  projects: Project[]
  bots: Bot[]
  /** active runs only */
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

/** `POST /api/hosts/:name/shells` 開的純 shell pane；`pane_id` 只在 daemon 這一輪有效。 */
export interface HostShell {
  host: string
  pane_id: string
  tab_id: string
  workspace_id: string
  /** 實際目錄，不一定等於要求的 */
  cwd: string
  created_at: string
}

export interface PromptResult {
  turn_id: string
  message_id: string | null
  /** `queued`＝daemon 收下了、還沒送（對方回合中的派工，或 issue #122 的「先收下再啟動」）。 */
  delivery: TurnDelivery | 'queued'
  /** 只有請求帶 `send_now` 時才有（issue #103）：`interrupted`＝打斷了一個回合，`idle`＝當下沒回合在飛，
   *  其他值（`send_now_*`）是**沒有**插隊的原因。 */
  send_now: string | null
}

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

/** daemon `WsEvent` */
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
    // daemon: `reason` (conflict) / `message` (bad request, upstream) / `what` (not found)。
    // 舊 daemon 對新路徑會回 405／404 且 body 是空的，欄位在但字串是空的；直接用會變「儲存失敗：」後面沒字，
    // 所以空字串一律退回帶狀態碼的 fallback。
    const detail = String(body.reason ?? body.message ?? body.what ?? body.error ?? '').trim()
    super(detail || fallback)
    this.name = 'ApiError'
    this.status = status
    this.body = body
  }
}

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
  /** 名稱撞到時 daemon 自動找 `<base>-<n>`；回應帶實際 `name`。 */
  name_auto?: boolean
  kind: BotKind
  /** `null` / 省略 = CLI 預設 */
  effort?: string | null
  model?: string | null
  /** 契約保留，UI 一律省略（使用者決定）；要調參數改 identity 或 config.toml。 */
  args?: string[]
  autostart: boolean
  auto_approve?: boolean
  identity?: string | null
  env?: Record<string, string>
  fast?: boolean
  persona?: string | null
  /** claude 才收；`null` / 省略 = `claude-md`。 */
  instruction_files?: InstructionFiles | null
  /** 冪等鍵（#352）：回應遺失後同一個動作重送，daemon 拿回第一次建好的那顆而不是再建一顆；見 `lib/createRequestId.ts`。 */
  client_request_id?: string
}

/** API.md v3.3：只送有變更的欄位；`null` 代表清除。 */
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
  /** claude user bot 才收；`null` 清回 `claude-md`。要重啟才讀到新值。 */
  instruction_files?: InstructionFiles | null
  /** 純顯示，`needs_restart` 一定是 false */
  primary?: boolean
}

/** `needs_restart = true`：已寫入 config，但目前的 Run 仍跑舊參數，要 restart 才生效。 */
export interface PatchBotResult {
  needs_restart: boolean
}

/** grok 的靜態 effort（`GET /api/models` 失敗時退回）。 */
export const EFFORT_OPTIONS = ['low', 'medium', 'high', 'xhigh'] as const
/** claude `--effort`（2.1+）；TUI 的 `/effort` 是拉桿、無帶參數形式，所以改了要重啟。 */
export const CLAUDE_EFFORT_OPTIONS = ['low', 'medium', 'high', 'xhigh', 'max'] as const
/** codex 的靜態 effort（API.md §12.2；`GET /api/models` 失敗時退回）。 */
export const CODEX_EFFORT_OPTIONS = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'] as const

/** 不翻譯，用原廠的字（只首字母大寫），才對得上 CLI 畫面與文件。 */
export function effortLabel(level: string): string {
  return level ? level.charAt(0).toUpperCase() + level.slice(1) : level
}

export const MODEL_OPTIONS: Record<BotKind, readonly string[]> = {
  // 由輕到重（2026-09-09 使用者決定）；`sortModels` 以此排序 API 清單。
  claude: ['haiku', 'sonnet', 'opus', 'fable'],
  // 順序同 claude（2026-09-09 使用者決定）；gpt-5.5 拿掉（2026-09-09）。
  codex: ['gpt-5.6-luna', 'gpt-5.6-terra', 'gpt-5.6-sol', 'gpt-6-astra'],
  // `grok models`（grok 1.0.13，2026-09-06）
  grok: ['grok-4.6', 'grok-4.5'],
}

/**
 * 選單隱藏的模型（2026-09-09）：codex `model/list` 仍回上一代。daemon 照抄 CLI，所以在選單層過濾；
 * 已設成它的 bot 仍保留那顆按鈕，否則會被顯示成「自訂」。
 */
export const HIDDEN_MODELS: readonly string[] = ['gpt-5.5']

/** `<select>` sentinel 值 */
export const MODEL_DEFAULT = ''
export const MODEL_CUSTOM = 'custom'

/** `GET /api/models?kind=&host=` */
export interface ModelInfo {
  id: string
  display_name: string
  description: string
  is_default: boolean
  /** null = 不適用 */
  default_effort: string | null
  /** 空陣列 = 該模型不支援 */
  efforts: string[]
  /** 含 `priority` 時 UI 才顯示「Fast」開關 */
  service_tiers: { id: string; name: string; description: string }[]
}

export const FAST_TIER = 'priority'

export interface QuotaWindow {
  used_pct: number
  /** ISO；可能為 null */
  resets_at: string | null
  /** daemon 算的門檻（剩 30%）；UI 不可自己用 `used_pct` 寫死比較。 */
  low: boolean
  /** daemon 算的門檻（剩 5%） */
  critical: boolean
}

/** `GET /api/mem` / WS `mem_updated`（SPEC §15） */
export interface HostMem {
  host: string
  herdr_bytes: number
  /** 底下的 pane / agent CLI */
  agents_bytes: number
  total_bytes: number
  processes: number
  /** 非 null 時數字都是 0，不是真的 0 */
  error: string | null
  /** Chromium 系瀏覽器（2026-09-08）；沒有就是空陣列 */
  browsers: BrowserMem[]
  /** 整台機器（2026-09-12），回答「還能不能再開 bot」；讀不出時為 null。 */
  machine: MachineMem | null
}

/** SPEC §15 */
export interface MachineMem {
  total_bytes: number
  /** Linux 取 MemAvailable；macOS 取 free+inactive+speculative+purgeable。 */
  available_bytes: number
}

export interface BrowserMem {
  name: string
  /** renderer process 數 ≈ 分頁數 */
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
  /** 每個專案的 bot（含 child）佔幾個 pane、多少 RAM（2026-09-15）；量不到的主機上的專案不在清單裡。 */
  projects: ProjectMem[]
}

export interface ProjectMem {
  project_id: string
  host: string
  panes: number
  bytes: number
}

/** `GET /api/mem/processes`（SPEC §15.2） */
export interface MemProcess {
  pid: number
  ppid: number
  rss_bytes: number
  /** 路徑已剝掉 */
  exe: string
  argv: string
  /** 讀不到環境時 null */
  pane_id: string | null
  /** pane id 是 per-session 的，要配這個 socket；讀不到時 null */
  socket_path: string | null
  /** bot 已刪也還在，此時 `bot_name` 為 null */
  bot_id: string | null
  bot_name: string | null
  project_id: string | null
  owner: MemOwner
  /** 自己＋子孫的 RSS（砍掉能省多少）；清單依它降冪 */
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

/** `GET /api/search/messages`：命中次數與最新一次的前後文。 */
export interface MessageHit {
  hits: number
  snippet: string
}

/** codex `rateLimitResetCredits`（TUI 的 `Reset usage`）：可立刻清空額度的券；其餘 kind 為 null。 */
export interface QuotaResetCredits {
  available: number
  /** 第一張可用券的名稱 */
  title: string | null
  /** ISO */
  expires_at: string | null
}

export interface KindQuota {
  five_hour: QuotaWindow | null
  seven_day: QuotaWindow | null
  /** Max 方案的 Fable 週額度；沒有就是 null */
  fable: QuotaWindow | null
  reset_credits: QuotaResetCredits | null
  /** CLI 的上限橫幅；codex credits 用完時 5h/7d 可以是滿的（2026-09-12），所以要單獨看。 */
  limit_hit: QuotaLimitHit | null
  plan: string | null
  updated_at: string
  /** daemon 重啟後從上一輪讀數回填；新的探測回來前仍可顯示但不是 fresh。 */
  stale: boolean
  /** `local` 或 `hosts[].name` */
  host: string
}

/** key 見 `quotaKey`（SPEC §14，按主機分開）；null = 沒有額度資訊。 */
export type QuotaMap = Record<string, KindQuota | null>

export interface QuotaLimitHit {
  message: string
  /** ISO；null = 沒寫，要等下一回合成功才消失 */
  until: string | null
  /** ISO */
  at: string
}

export interface InstallToolResult {
  turn_id: string
}

/** 列表不含完整 body */
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

export interface IssueDetail extends Issue {
  body: string
}

// 群組任務（mission，docs/API.md「群組任務」）

/** D2 */
export type MissionDelivery = 'push_main' | 'pr'

/** D5：5h 撞限時等重置或換身分；7d 撞限一律換。 */
export type MissionOn5h = 'wait' | 'switch'

/** 由欄位推導：`cancelled_at → done_at → paused_reason → open`。 */
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
  /** 使用者追問（已完成的任務也能問） */
  | 'question'
  /** 使用者回答暫停，或 AGM 回答 `question`（`reply_to`） */
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
  /** `max_rounds` / `no_fable_for_verifier` / `push_main_failed` / `pr_failed`，或呼叫端自訂。 */
  paused_reason: string | null
  paused_detail: string | null
  result_summary: string | null
  /** 非 null ＝ 續作 */
  parent_mission_id: string | null
  status: MissionStatus
  /** P1b：daemon 由 assignments 推導；`null` = 舊 daemon 沒給，前端從事件推。 */
  phase: MissionPhaseServer | null
  created_at: string
  updated_at: string
  completed_at: string | null
  cancelled_at: string | null
}

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
  /** `answer` 指回 `question` 的事件 id；其他為 null */
  reply_to: string | null
  created_at: string
}

/** `answer` 為 null ＝ 還在等 AGM 回 */
export interface MissionQna {
  question: MissionEvent
  answer: MissionEvent | null
}

/** P1b：「誰在跑」「為什麼換手」以 `role` / `turn_error` 為準，別從事件 payload 猜。 */
export interface MissionAssignment {
  id: string
  role: MissionRole | null
  status: string
  target_bot_id: string | null
  turn_status: string | null
  /** 撞限時抄自 `run.turn_error` */
  turn_error: string | null
  /** 撞限換手時接手的來源 */
  follow_up_of: string | null
  /** `quota_blocked` 時 controller 預計重送的時間；舊 daemon 沒給為 null */
  resume_at: string | null
  created_at: string
  completed_at: string | null
}

export interface MissionDetail extends Mission {
  events: MissionEvent[]
  /** 舊的在前；舊 daemon 為空陣列 */
  assignments: MissionAssignment[]
  /** 新的在前；舊 daemon 為空陣列 */
  revisions: MissionRevisionRef[]
  /** `missing` ＝ 來源任務已經不在 */
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

/** `POST /api/projects/:id/missions`（§11） */
export interface NewMissionInput {
  text: string
  /** 省略時由 store 的 `missionRequests` 給：重送要沿用同一個，才吃得到 daemon 的冪等。 */
  client_request_id?: string
  delivery_mode: MissionDelivery
  executor_kind: BotKind
  on_5h_limit: MissionOn5h
  max_rounds?: number
}

/** `GET/POST /api/claude-update/review` 的 `review` 欄位：這一版的 AGM 解析到哪了。 */
export interface ClaudeReview {
  state: 'none' | 'pending' | 'done'
  target_bot_name: string
  asked_at: string
  answered_at: string
  /** `done` 才有：AGM 的結論原文。 */
  result: string
}
