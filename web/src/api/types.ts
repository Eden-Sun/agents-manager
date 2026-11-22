import { msg, t, tr } from '../lib/i18n'

export type BotKind = 'claude' | 'codex' | 'grok' | 'agy' | 'claude-api' | 'openai-api'
export const BOT_KINDS: readonly BotKind[] = ['claude', 'codex', 'grok', 'agy']
export const API_BOT_KINDS: readonly BotKind[] = ['claude-api', 'openai-api']
export const CREATABLE_KINDS: readonly BotKind[] = [...BOT_KINDS, ...API_BOT_KINDS]
export const API_BOT_KIND: BotKind = 'claude-api'

export type RunState = 'starting' | 'running' | 'stopping' | 'stopped' | 'exited'
export type AgentStatus = 'idle' | 'working' | 'blocked' | 'unknown'
export type TurnStatus = 'queued' | 'in_flight' | 'completed' | 'completed_fallback' | 'failed'
export type TurnDelivery = 'pending' | 'ok' | 'unknown' | 'failed'
export type TurnOrigin = 'web' | 'external'
export type MessageRole = 'user' | 'assistant' | 'system'
export type MessageSource = 'web' | 'hook' | 'transcript' | 'terminal_fallback' | 'system' | 'share'
export type TerminalSource = 'visible' | 'recent_unwrapped'

export interface Project {
  mode?: 'parent_child' | 'expert'
  id: string
  path: string
  label: string
  workspace_id: string | null
  host: string
  github: ProjectGithub | null
  handed_off_to?: string | null
  role_policy?: Record<string, RuntimeSettings> | null
  deputy?: ProjectDeputy | null
  specialist_dispatch?: boolean
  created_at: string
}

export interface ProjectDeputy {
  bot_id: string
  after_sec?: number | null
}

export interface ProjectGithub {
  owner: string
  repo: string
  url: string
}

export interface ProjectSubmodule {
  path: string
  github: ProjectGithub | null
}

export interface HerdrVersion {
  server_version: string | null
  protocol: number | null
  protocol_supported: boolean | null
  cli_version: string | null
  mismatch: boolean
}

export const UNKNOWN_HERDR: HerdrVersion = { server_version: null, protocol: null, protocol_supported: null, cli_version: null, mismatch: false }

export interface BaselineIssue {
  id: string
  severity: 'critical' | 'warn'
  message: string
  note?: string
}

export interface HostBaseline {
  issues: BaselineIssue[] | null
  checked_at: string
  os: string | null
  stale: boolean
  error: string | null
  fixable?: string[]
}

export interface BaselineAction {
  id: string
  action: 'merge_key' | 'manual' | string
  severity: 'critical' | 'warn'
  target: string
  key?: string
  value?: unknown
  reason?: string
}

export interface HostBaselinePlan {
  host: string
  checked_at: string
  stale: boolean
  plan_rev: string
  actions: BaselineAction[]
}

export interface BaselineApplyItemResult {
  id: string
  result: 'applied' | 'present' | 'skipped' | 'manual' | string
  reason?: string
}

export interface HostBaselineApplyResult {
  host: string
  plan_rev: string
  backup_id: string | null
  error: string | null
  results: BaselineApplyItemResult[]
}

export interface HostBaselineBackupFile {
  file: string
  restore: 'overwrite' | 'delete'
}

export interface HostBaselineBackup {
  backup_id: string
  created_at: string
  files: HostBaselineBackupFile[]
}

export interface HostBaselineBackups {
  backups: HostBaselineBackup[]
  skipped: number
}

export interface HostBaselineRestoreResult {
  backup_id: string
  files: Array<{ file: string; result: 'restored' | 'deleted' | 'absent' | 'failed' }>
}

export interface Host {
  name: string
  ssh: string
  ssh_port: number
  herdr_session: string
  remote_path: string
  connected: boolean
  error: string | null
  disconnected_since: string | null
  shared_session?: boolean
  attach_command: string
  herdr: HerdrVersion
  tools: ToolMap
  baseline: HostBaseline | null
  identity_status: IdentityStatusMap
  claude_login_expires_at?: Record<string, number> | null
}

export interface ToolStatus {
  installed: boolean
  path: string | null
  version: string | null
  logged_in: boolean | null
  quota_error?: { reason: string; message: string; at: string }
}

export type ToolMap = Record<BotKind, ToolStatus>

export const TOOL_UNKNOWN: ToolStatus = { installed: true, path: null, version: null, logged_in: null }

export interface IdentityStatus {
  name: string
  kind: BotKind
  logged_in: boolean | null
  reason: string | null
  checked_at?: string | null
  basis?: string | null
  account: string | null
  plan: string | null
  source: 'config' | 'shell'
  config_dir: string | null
  login_needed?: { since: string; via: string } | null
}

export type IdentityStatusMap = Record<string, IdentityStatus>

export const LOCAL_HOST = 'local'

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
  ssh_opts?: string[]
}

export interface HostResult {
  name: string
  connected: boolean
  error: string | null
}

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
  password?: string
}

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
  logged_in: boolean | null
  account: string | null
  accounts: GhAccount[]
  mode: string | null
  pending: GhPending | null
  error: string | null
}

export const HOST_DEFAULTS = {
  ssh_port: 22,
  herdr_session: 'agents-manager',
  remote_path: '/opt/homebrew/bin:$HOME/.local/bin',
} as const

export interface RuntimeSettings {
  context_tokens?: number | null
  keep_warm?: boolean | null
  keep_warm_after_min?: number | null
  hot_compact_after_min?: number | null
  hot_compact_min_pct?: number | null
}

export interface Bot {
  scheduled?: boolean
  role?: 'manager' | 'executor' | 'advisor' | 'specialist' | 'scheduler' | null
  track?: string | null
  runtime_override?: RuntimeSettings | null
  role_pending?: boolean
  policy_pending?: boolean
  daemon_url_pending?: boolean
  runtime_role_policy?: string | null
  manager_use?: boolean
  cluster?: string | null
  id: string
  project_id: string
  name: string
  kind: BotKind
  model: string | null
  effort: string | null
  fast: boolean
  persona: string | null
  args: string[]
  autostart: boolean
  inject_hooks: boolean
  auto_approve: boolean
  identity: string | null
  env: Record<string, string>
  herdr_session: string | null
  managed_by: BotManagedBy
  parent_bot_id: string | null
  primary: boolean
  primary_position: number
  limit_hit?: BotLimitHit | null
  needs_restart: boolean
  live_apply_deferred?: boolean
  cwd: string | null
  share_profile: ShareProfile | null
  share_enabled: boolean
  preview?: { status: 'off' | 'starting' | 'running' | 'failed'; port: number | null } | null
  pending?: boolean
  create_request_id?: string | null
  agent_name: string | null
  unread?: number
  read_mark?: { at: string; id: string; seq?: number } | null
  created_at: string
}

export type BotManagedBy = 'user' | 'child'

export interface Identity {
  name: string
  host?: string | null
  kind: BotKind
  env: Record<string, string>
  args: string[]
}

export interface NewIdentityInput {
  name: string
  kind: BotKind
  host?: string
  env: Record<string, string>
  args?: string[]
}

export interface IdentitySyncItem {
  name: string
  kind: BotKind
  source: 'shell' | 'config'
  status: 'missing' | 'different'
  can_apply: boolean
  local_env: Record<string, string>
  remote_env: Record<string, string> | null
  alias_line: string | null
  note: string | null
}

export interface IdentitySyncPlan {
  host: string
  remote_shell: string | null
  remote_rc: string | null
  already_synced: number
  items: IdentitySyncItem[]
}

export interface IdentitySyncResultItem {
  name: string
  kind: BotKind
  source: 'shell' | 'config'
  status: 'created' | 'would_create' | 'skipped' | 'failed'
  reason: string | null
  message: string | null
  alias_line: string | null
  created_dirs: string[]
}

export interface IdentitySyncResponse {
  host: string
  dry_run: boolean
  backup_path: string | null
  results: IdentitySyncResultItem[]
}

export type Lamp = 'disconnected' | 'offline' | 'starting' | 'stopping' | 'idle' | 'working' | 'blocked' | 'unknown'

export interface BlockedReason {
  code: string
  text: string
}

export interface Run {
  send_now_supported?: boolean
  id: string
  bot_id: string
  state: RunState
  agent_status: AgentStatus
  workspace_id: string | null
  pane_id: string | null
  adopted: boolean
  herdr_session: string | null
  agent_title: string | null
  status_line: string | null
  status: StatusInfo | null
  update_notice: string | null
  background_jobs?: number | null
  background_since?: string | null
  background_stuck?: boolean
  blocked_reason?: BlockedReason | null
  prompt_suggestion?: string | null
  compacting_since?: string | null
  background_tasks?: BackgroundTask[] | null
  session_crons?: SessionCron[] | null
  turn_error: string | null
  turn_error_kind?: string | null
  runtime_model: string | null
  runtime_effort: string | null
  runtime_fast: boolean | null
  runtime_identity: string | null
  native_session_id: string | null
  transcript_path: string | null
  started_at: string
  ended_at: string | null
  agent_status_since: string | null
  last_api_at?: string | null
  cache_ttl_secs?: number | null
  cache_kept_warm_at?: string | null
  cache_compacted?: boolean
  keep_warm_skip?: boolean
  keep_warm_replied_at?: string | null
  prompt_cache?: PromptCacheInfo | null
}

export interface RestartSkip {
  bot_id: string
  name: string
  reason: string
  reason_label: string
}

export interface RestartPlan {
  batch_id: string
  total: number
  planned: { bot_id: string; name: string }[]
  skipped: RestartSkip[]
  already_running: boolean
  restart_status?: string
}

export interface RestartBatch {
  id: string
  total: number
  done: number
  current: string | null
  ok: string[]
  failed: { name: string; error: string }[]
  skipped: RestartSkip[]
  finished: boolean
}

export interface StatusInfo {
  account_email: string | null
  account_warning: string | null
  model_name: string | null
  model_id: string | null
  effort: string | null
  thinking: boolean
  fast_mode: boolean | null
  context_used_pct: number | null
  context_used_tokens: number | null
  context_size: number | null
  context_unknown: boolean
  five_hour_pct: number | null
  five_hour_resets_at: number | null
  seven_day_pct: number | null
  seven_day_resets_at: number | null
  cost_usd: number | null
  cwd: string | null
  version: string | null
  session_name: string | null
  prompt_cache?: PromptCacheInfo | null
}

export interface PromptCacheInfo {
  warm: boolean | null
  expires_at: number | null
  ttl_secs: number | null
  recache_tokens_if_cold: number | null
  caching_observed: boolean | null
  source: 'statusline' | 'rollout_estimate' | null
  hit_ratio: number | null
  context_used_pct: number | null
  context_used_tokens: number | null
  context_size: number | null
}

export interface Turn {
  id: string
  conversation_id: string
  run_id: string | null
  bot_id?: string | null
  origin: TurnOrigin
  status: TurnStatus
  delivery: TurnDelivery
  unverified: boolean
  autoResend: boolean
  awaitsStart: boolean
  awaitsIdle: boolean
  startError: string | null
  client_request_id: string | null
  created_at: string
  completed_at: string | null
}

export interface Attachment {
  id: string
  name: string
  mime: string
  size: number
  path: string
}

export type LedgerItemStatus = 'untriaged' | 'answered' | 'following' | 'clarifying' | 'done' | 'cancelled' | 'merged'

export type LedgerDeployState = 'pending' | 'live'

export interface LedgerItem {
  id: string
  code: string
  code_no: number
  project_id: string
  source_message_id: string | null
  ordinal: number
  title: string
  status: LedgerItemStatus
  owner_bot_id: string | null
  deliverable: string
  next_step: string
  wait_for: string | null
  due_at: string | null
  merged_into_id: string | null
  close_reason: string | null
  deploy_sha?: string | null
  deploy_state?: LedgerDeployState | null
  version: number
  updated_at: string
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
  group_id: string | null
  attachments: Attachment[]
  relay_from: string | null
  relay_unverified?: boolean
  relay_from_name?: string | null
  via_cluster?: string | null
  assignee_bot_id?: string | null
  assignee_name?: string | null
  ledger_items?: LedgerItem[]
  relay_from_deleted?: boolean
  terminal_snapshot: string | null
  created_at: string
  rewound_at?: string | null
  sent_via?: 'send_now' | 'supplement' | null
  seq?: number
  keep_warm?: boolean
}

export interface AppState {
  codex_reset?: unknown
  daemon_seq: number
  restart_batch: string | null | undefined
  cli_updates: { update_id: string; host: string }[] | undefined
  herdr_updates: { update_id: string; host: string; target_version: string; phase: string; started_at: string }[] | undefined
  deploy_wait: unknown
  connected: boolean
  default_connected: boolean
  attach_command: string
  herdr: HerdrVersion
  tools: ToolMap
  baseline: HostBaseline | null
  host_baseline_sync: Record<string, 'off' | 'ask' | 'auto'>
  identity_status: IdentityStatusMap
  claude_login_expires_at?: Record<string, number> | null
  hosts: Host[]
  identities: Identity[]
  projects: Project[]
  bots: Bot[]
  runs: Run[]
  turns: Turn[]
}

export interface TerminalSnapshot {
  run_id?: string | null
  text: string
  revision: number | null
  truncated: boolean
  source: TerminalSource
  pane_id: string | null
  columns: number | null
  rows: number | null
  format?: TerminalFormat
}

export type TerminalFormat = 'text' | 'ansi'

export interface ShellStreamFrame {
  seq: number
  text: string
  cols: number | null
  rows: number | null
}

export interface HostShell {
  host: string
  pane_id: string
  tab_id: string
  workspace_id: string
  cwd: string
  created_at: string
}

export interface PromptResult {
  turn_id: string
  message_id: string | null
  delivery: TurnDelivery | 'queued'
  send_now: string | null
}

export interface WithdrawTurnResult {
  text: string
  attachments: string[]
}

export interface MessagesPage {
  bot_id: string
  conversation_id: string
  messages: Message[]
  turns: Turn[]
  has_more: boolean
}

export interface ApiErrorBody {
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
  host?: string
}

export interface PatchProjectInput {
  mode?: 'parent_child' | 'expert'
  manager_bot_id?: string
  label?: string
  handed_off_to?: string | null
  role_policy?: Record<string, RuntimeSettings> | null
  deputy?: ProjectDeputy | null
  specialist_dispatch?: boolean
}

export interface DirEntry {
  name: string
  path: string
  git: boolean
}

export interface CreatedDir {
  path: string
  name: string
  parent: string | null
}

export interface DirListing {
  path: string
  parent: string | null
  home: string
  entries: DirEntry[]
  truncated?: boolean
}

export type ShareProfile = 'restricted' | 'trusted'

export type ShareFolderIn = { kind: 'new'; name: string } | { kind: 'existing'; path: string }

export interface NewBotInput {
  name: string
  role?: 'scheduler' | 'executor' | 'advisor'
  name_auto?: boolean
  kind: BotKind
  effort?: string | null
  model?: string | null
  args?: string[]
  autostart: boolean
  auto_approve?: boolean
  identity?: string | null
  env?: Record<string, string>
  fast?: boolean
  persona?: string | null
  share_profile?: ShareProfile
  share_folder?: ShareFolderIn
  confirm_trusted?: boolean
  client_request_id?: string
}

export interface PatchBotInput {
  runtime_override?: RuntimeSettings | null
  role?: 'manager' | 'executor' | 'advisor' | 'specialist' | 'scheduler' | null
  track?: string | null
  manager_use?: boolean
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
  primary?: boolean
}

export interface ModelRemap {
  from: string
  to: string
}

export interface PatchBotResult {
  needs_restart: boolean
  live_apply?: { applied: boolean; deferred: boolean;  interrupted: boolean; reason: string | null }
  remapped_model: ModelRemap | null
  message?: string | null
}

export const EFFORT_OPTIONS = ['low', 'medium', 'high', 'xhigh'] as const
export const CLAUDE_EFFORT_OPTIONS = ['low', 'medium', 'high', 'xhigh', 'max'] as const
export const CODEX_EFFORT_OPTIONS = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'] as const
export const OPENAI_API_EFFORT_OPTIONS = ['none', 'low', 'medium', 'high', 'xhigh', 'max'] as const
export const OPENAI_API_NO_NONE_EFFORT_OPTIONS = ['low', 'medium', 'high', 'xhigh', 'max'] as const
export function openAiApiEffortsFor(model: string | null | undefined): readonly string[] {
  return model === 'gpt-6-luna' ? OPENAI_API_EFFORT_OPTIONS : OPENAI_API_NO_NONE_EFFORT_OPTIONS
}

export function effortLabel(level: string): string {
  if (level === 'default') return t('（CLI 預設）')
  return level ? level.charAt(0).toUpperCase() + level.slice(1) : level
}

export function effortShort(level: string): string {
  const short: Record<string, string> = { low: 'L', medium: 'M', high: 'H', xhigh: 'XH', minimal: 'Min', none: '-', default: msg('預設') }
  const label = short[level.toLowerCase()]
  return label ? tr(label) : effortLabel(level)
}

export const AGY_DEFAULT_MODEL = 'gemini-3.8-flash-medium'

export const MODEL_OPTIONS: Record<BotKind, readonly string[]> = {
  claude: ['haiku', 'sonnet', 'claude-opus-5-5', 'fable'],
  'claude-api': ['claude-opus-5-5', 'claude-fable-5-2'],
  'openai-api': ['gpt-6-luna', 'gpt-6.2-sol', 'gpt-6.2-astra'],
  codex: ['gpt-6-luna', 'gpt-6-sol', 'gpt-6-astra'],
  grok: ['grok-4.7', 'grok-4.7-build-fast', 'grok-4.6', 'grok-4.5'],
  agy: [
    'gemini-3.8-flash-medium',
    'gemini-3.8-flash-high',
    'gemini-3.8-flash-low',
  ],
}

export const HIDDEN_MODELS: readonly string[] = ['gpt-5.5', 'gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna']

export const DEPRECATED_MODELS: Readonly<Record<string, string>> = {
  'claude:opus': 'claude-opus-5-5',
  'codex:gpt-5.6-sol': 'gpt-6-sol',
  'codex:gpt-5.6-terra': 'gpt-6-sol',
  'codex:gpt-5.6-luna': 'gpt-6-luna',
}

export function agyModelAllowed(id: string): boolean {
  if (/^gemini-pro-orchestrator-[a-z0-9-]+$/.test(id)) return true
  const m = /^gemini-(\d+)(?:\.(\d+))?(?=-|$)/.exec(id)
  if (!m) return false
  const major = Number(m[1])
  const minor = Number(m[2] ?? 0)
  return major > 3 || (major === 3 && minor >= 8)
}

export function canonicalModel(kind: BotKind, id: string): string {
  if (kind === 'agy' && !agyModelAllowed(id)) return AGY_DEFAULT_MODEL
  return DEPRECATED_MODELS[`${kind}:${id}`] ?? id
}

export interface ModelsMeta {
  source?: string | null
  source_host?: string | null
  stale?: boolean
}

export interface ModelInfo {
  id: string
  display_name: string
  description: string
  is_default: boolean
  default_effort: string | null
  account_default_effort?: string | null
  efforts: string[]
  context_window?: number | null
  service_tiers: { id: string; name: string; description: string }[]
}

export const FAST_TIER = 'priority'

export interface QuotaWindow {
  used_pct: number
  resets_at: string | null
  observed_at: string | null
  low: boolean
  critical: boolean
}

export interface HostMem {
  host: string
  herdr_bytes: number
  agents_bytes: number
  total_bytes: number
  processes: number
  error: string | null
  browsers: BrowserMem[]
  machine: MachineMem | null
}

export interface MachineMem {
  total_bytes: number
  available_bytes: number
}

export interface BrowserMem {
  name: string
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
  projects: ProjectMem[]
}

export interface ProjectMem {
  project_id: string
  host: string
  panes: number
  bytes: number
}

export interface MemProcess {
  pid: number
  ppid: number
  rss_bytes: number
  exe: string
  argv: string
  pane_id: string | null
  socket_path: string | null
  bot_id: string | null
  bot_name: string | null
  project_id: string | null
  owner: MemOwner
  subtree_bytes: number
  children: number
}

export type MemOwner = 'bot' | 'pane' | 'herdr' | 'unknown'

export interface MemProcesses {
  host: string
  sampled_at: string
  processes: MemProcess[]
}

export interface MessageHit {
  hits: number
  snippet: string
}

export interface QuotaResetCredits {
  available: number
  title: string | null
  expires_at: string | null
  credits: QuotaResetCredit[]
}

export interface QuotaResetCredit {
  id: string
  title: string | null
  status: string
  granted_at: string | null
  expires_at: string | null
}

export interface KindQuota {
  five_hour: QuotaWindow | null
  seven_day: QuotaWindow | null
  fable: QuotaWindow | null
  argon_weekly?: QuotaWindow | null
  reset_credits: QuotaResetCredits | null
  limit_hit: QuotaLimitHit | null
  plan: string | null
  updated_at: string
  stale: boolean
  host: string
  held_bots?: string[]
  reset_offer_snoozed_until?: string | null
}

export type QuotaMap = Record<string, KindQuota | null>

export interface BotLimitHit {
  message: string
  until: string | null
}

export interface QuotaLimitHit {
  message: string
  until: string | null
  at: string
  bucket: string | null
}

export interface InstallToolResult {
  turn_id: string
}

export interface Issue {
  number: number
  title: string
  content_notice?: string
  state: 'open' | 'closed'
  labels: IssueLabel[]
  url: string
  updated_at: string
  author: string
  body_excerpt: string
}

export interface IssueLabel {
  name: string
  color: string | null
}

export interface IssueDetail extends Issue {
  body: string
}

export interface UpdateReview {
  state: 'none' | 'pending' | 'done' | 'failed'
  target_bot_name: string
  asked_at: string
  answered_at: string
  result: string
  source: '' | 'api'
  model: string
  error?: string
}

export interface BackgroundTask {
  id: string
  type: string
  status: string
  description: string
  command?: string
}

export interface SessionCron {
  id: string
  schedule: string
  recurring: boolean
  prompt: string
}
