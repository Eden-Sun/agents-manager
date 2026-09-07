/**
 * In-memory daemon simulator, enabled with `VITE_MOCK=1`.
 *
 * It implements the SPEC §7 REST surface and pushes the SPEC §7.3 WebSocket events so the
 * UI can be developed and screenshotted without the Rust daemon. Timings are compressed.
 *
 * Prompt keywords that steer the simulation (documented in docs/FRONTEND.md):
 *   - text containing `blocked` (or `rm -rf`) → the agent goes `blocked` and waits for keys
 *   - text containing `fallback`             → the reply arrives as `terminal_fallback` (incomplete)
 *   - text containing `slow`                 → the reply takes ~8s (good for testing the composer lock)
 *
 * Dev helpers on `window.__amMock`: `resync()`, `dropSocket()`, `block(botId)`, `disconnect()`,
 * `hostDown(name)`, `hostUp(name)` (SPEC §11.6 remote hosts), `paneSqueeze(n)` /
 * `paneMoveOff()` / `paneMoveOn()`（窄 pane 警示與「移到自己的分頁」的退回路徑）、
 * `hostShellsOff()` / `hostShellsOn()`（主機 shell 的缺端點退回路徑）。
 */

import { parseMentions } from './mentions'
import { ApiError, BOT_KINDS, TEAM_BUDGET_DEFAULTS, TEAM_WORKERS_MAX } from './types'
import type { BotKind, TeamBudget, TeamDeliver, TeamPhase, TeamRole, TeamTaskState } from './types'
import type { HttpMethod, SocketHandlers, Transport } from './transport'

/** 未知 kind 一律當 claude（與 daemon 的 400 不同，mock 寬鬆處理）。 */
function toKind(v: unknown): BotKind {
  return BOT_KINDS.includes(v as BotKind) ? (v as BotKind) : 'claude'
}

type Rec = Record<string, unknown>

let counter = 0
function ulid(prefix: string): string {
  counter += 1
  return `${prefix}_${Date.now().toString(36)}${counter.toString(36).padStart(3, '0')}`
}
const now = () => new Date().toISOString()

/**
 * herdr workspace 的總欄數（實測 2026-09-06 的那台是 185）。同一個分頁裡的 pane 平分它，
 * 自己獨佔一個分頁的就全拿——`pane.move → new_tab` 之所以有效就是因為分頁之間不互搶。
 */
const WORKSPACE_COLUMNS = 185

interface MockRun {
  id: string
  bot_id: string
  state: 'starting' | 'running' | 'stopping' | 'stopped' | 'exited'
  agent_status: 'idle' | 'working' | 'blocked' | 'unknown'
  workspace_id: string | null
  pane_id: string | null
  /** pane 佔著自己的分頁（`pane.move → new_tab` 之後）；false = 跟其他 pane 擠預設分頁。 */
  own_tab: boolean
  adopted: number
  native_session_id: string | null
  transcript_path: string | null
  /** claude 的 statusLine hook payload（`runs.status_json` 的形狀，見 normalize.toStatusInfo）。 */
  status: Record<string, unknown> | null
  /** pane 上那一行被終端寬度壓縮過的原文，UI 拿它當 tooltip / fallback。 */
  status_line: string | null
  started_at: string
  ended_at: string | null
}

/**
 * A claude statusLine payload, shaped like the real hook's JSON (`normalize.toStatusInfo`
 * parses this exact tree). Values mirror a real session so the status bar is exercised at
 * a realistic width rather than with `1%` placeholders.
 */
function claudeStatusJson(cwd: string): Record<string, unknown> {
  const inHours = (h: number) => Math.floor(Date.now() / 1000) + h * 3600
  return {
    account_email: 'tony.lin@robinstech.com.tw',
    model: { id: 'claude-opus-5', display_name: 'Opus 5' },
    effort: { level: 'high' },
    thinking: { enabled: true },
    context_window: { used_percentage: 26, total_input_tokens: 263000, context_window_size: 1000000 },
    rate_limits: {
      five_hour: { used_percentage: 85, resets_at: inHours(0.17) },
      seven_day: { used_percentage: 27, resets_at: inHours(20) },
    },
    cost: { total_cost_usd: 18.67 },
    workspace: { current_dir: cwd },
    version: '2.1.263',
  }
}

interface MockTurn {
  id: string
  conversation_id: string
  run_id: string
  bot_id: string
  origin: 'web' | 'external'
  status: 'in_flight' | 'completed' | 'completed_fallback' | 'failed'
  delivery: 'pending' | 'ok' | 'unknown' | 'failed'
  client_request_id: string | null
  created_at: string
  completed_at: string | null
}

interface MockMessage {
  id: string
  conversation_id: string
  turn_id: string | null
  bot_id: string
  role: 'user' | 'assistant' | 'system'
  content: string
  source: 'web' | 'hook' | 'transcript' | 'terminal_fallback' | 'system'
  incomplete: number
  /** SPEC §13：群組發言的 group_id；一般訊息為 null */
  group_id: string | null
  /** 拖放進來的圖片（mock 只保留 metadata，位元組留在 `blobs`）。 */
  attachments_json: string | null
  /** SPEC-team §2.1：屬於哪個 team（null = 一般訊息）。 */
  team_id: string | null
  /** SPEC-team §2.1：這則 relay 的來源 bot（null = 使用者 / daemon 自己）。 */
  relay_from: string | null
  created_at: string
}

interface MockBot {
  id: string
  project_id: string
  name: string
  kind: BotKind
  /** API.md v3.3：模型別名，null = 不帶 `--model` */
  model: string | null
  effort: string | null
  /** v4.0 codex fast tier */
  fast: number
  /** v4.0 persona */
  persona: string | null
  args_json: string
  autostart: number
  inject_hooks: number
  auto_approve: number
  identity: string | null
  env_json: string
  /** SPEC-team §2.1 */
  managed_by: 'user' | 'team'
  team_id: string | null
  team_role: TeamRole | null
  cwd: string | null
  created_at: string
}

interface MockIdentity {
  name: string
  kind: BotKind
  env: Record<string, string>
  args: string[]
}

interface MockProject {
  id: string
  path: string
  label: string
  workspace_id: string | null
  host: string
  /** v4.0: GitHub remote, null = not a GitHub project */
  github: { owner: string; repo: string; url: string } | null
  created_at: string
}

/** v4.0 fake `gh issue list` for the seeded project. */
const ISSUES: Rec[] = [
  { number: 42, title: '群組聊天：刪掉的 bot 歷史不再出現在合併時間軸', state: 'open', labels: [{ name: 'bug', color: 'd73a4a' }, { name: 'group-chat', color: '0e8a16' }], author: 'edansun', updated_at: inHours(-3), body: '重現步驟：\n1. 在群組視圖送 `@all` \n2. 刪掉其中一個 bot\n3. 重新整理\n\n預期：歷史仍在；實際：只剩存活 bot 的訊息。\n\n相關：`GET /projects/:id/messages` 只合併現存 bot。' },
  { number: 57, title: '追加 issue 後繼續 Team', state: 'open', labels: [{ name: 'enhancement', color: 'a2eeef' }, { name: 'team', color: '5319e7' }], author: 'edansun', updated_at: inHours(-1), body: 'done 的 Team 可以追加下一個 issue，沿用 PM 與 reviewer 的對話。' },
  { number: 58, title: '為 Team reopen 補上事件紀錄', state: 'open', labels: [{ name: 'test', color: '7057ff' }], author: 'edansun', updated_at: inHours(-2), body: '追加 issue 時要在 timeline 留下 team_reopened。' },
  { number: 41, title: '即時輸出前幾幀是 TUI 雜訊（✢ Improvising…）', state: 'open', labels: [{ name: 'daemon', color: '1d76db' }, { name: 'polish', color: 'fbca04' }], author: 'edansun', updated_at: inHours(-9), body: '`turn_progress` 的前 1–2 幀會帶 Claude Code 的 spinner 文字，應在 daemon 端過濾。' },
  { number: 40, title: 'Bot 設定：codex 模型 / effort / fast 從 `GET /api/models` 取', state: 'open', labels: [{ name: 'enhancement', color: 'a2eeef' }, { name: 'web', color: '5319e7' }], author: 'edansun', updated_at: inHours(-20), body: '目前是靜態清單。改為 API 驅動，失敗退回靜態清單。' },
  { number: 39, title: '主機面板顯示三個 kind 的工具偵測徽章', state: 'open', labels: [{ name: 'enhancement', color: 'a2eeef' }], author: 'm4p-bot', updated_at: inHours(-30), body: '已安裝 ✓ / 未安裝 ✗ / 未登入 !，附「安裝」按鈕。' },
  { number: 38, title: '額度徽章：低於 20% 用警示色', state: 'open', labels: [{ name: 'web', color: '5319e7' }, { name: 'good first issue', color: '7057ff' }], author: 'edansun', updated_at: inHours(-50), body: '剩餘 = 100 − used_pct。' },
  { number: 37, title: '長訊息不要預先收合', state: 'closed', labels: [{ name: 'web', color: '5319e7' }], author: 'edansun', updated_at: inHours(-60), body: '已在 v3.9 UI polish 移除 isLong / clamped。' },
  { number: 35, title: 'hash 式 herdr agent name', state: 'closed', labels: [{ name: 'daemon', color: '1d76db' }], author: 'edansun', updated_at: inHours(-100), body: '`<project slug>-<bot id 尾 6 碼>`。' },
  { number: 33, title: 'grok kind：hook 不走 argv', state: 'closed', labels: [{ name: 'daemon', color: '1d76db' }, { name: 'grok', color: '000000' }], author: 'edansun', updated_at: inHours(-140), body: '改寫入 `<GROK_HOME>/hooks/agents-manager.json`。' },
]

// ------------------------------------------------------------ SPEC-team（mock）

interface MockTeam {
  id: string
  project_id: string
  issue_number: number
  issue_title: string
  issue_url: string
  phase: TeamPhase
  pause_reason: string | null
  resume_phase: TeamPhase | null
  base_ref: string
  base_sha: string
  branch: string
  worktree_root: string
  deliver: TeamDeliver
  supervised: boolean
  budget: TeamBudget
  usage: { relays: number; review_rounds_total: number; elapsed_min: number; per_bot: Record<string, { turns: number }> }
  members: { bot_id: string; role: TeamRole }[]
  issues: MockTeamIssue[]
  pr_url: string | null
  summary: string | null
  /** SPEC-team §10.7：使用者從這個 team 關掉 issue 的時間（mock 不碰真的 GitHub）。 */
  issue_closed_at: string | null
  created_at: string
  started_at: string | null
  ended_at: string | null
}

interface MockTeamIssue {
  id: string
  seq: number
  issue_number: number
  issue_title: string
  issue_url: string
  state: 'queued' | 'working' | 'done' | 'failed' | 'skipped'
  branch: string | null
  summary: string | null
  pr_url: string | null
  issue_closed_at: string | null
  fail_reason: string | null
  started_at: string | null
  ended_at: string | null
}

interface MockTeamTask {
  id: string
  team_id: string
  seq: number
  title: string
  brief: string
  files: string[]
  worker_bot_id: string
  branch: string
  state: TeamTaskState
  round: number
  last_report: string | null
  last_verdict: string | null
  merge_sha: string | null
  created_at: string
  updated_at: string
}

interface MockTeamEvent {
  id: string
  team_id: string
  kind: 'relay' | 'phase' | 'merge' | 'note' | 'user'
  from_bot_id: string | null
  to_bot_id: string | null
  task_id: string | null
  turn_id: string | null
  status: 'pending' | 'delivered' | 'dropped' | null
  payload: Rec
  created_at: string
}

/**
 * daemon 自己發的 relay（首則指派、合併通知、修復提示）在 `messages.relay_from` 上放的哨符。
 *
 * SPEC-team §2.1 把 `relay_from = NULL` 同時當成「使用者」與「daemon 自己」，前端因此分不出
 * 「你 → pm」跟「daemon → pm」。這裡用一個非 bot_id 的字串，UI 認不出來就顯示成 daemon。
 */
const DAEMON_SENDER = 'daemon'

/** 一行 am-team fenced 區塊（UI 會把它折成 chip）。 */
function amTeam(obj: Rec): string {
  return '```am-team\n' + JSON.stringify(obj, null, 2) + '\n```'
}

/** 假的 task 題材，依序取用。 */
const TEAM_TASK_SEEDS = [
  {
    title: '`GET /projects/:id/messages` 保留已刪除 bot 的歷史',
    brief: '改成用 `messages.bot_id` 直接查，不要 inner join `bots`；刪掉的 bot 以 bot_id 當顯示名。',
    files: ['daemon/src/group.rs'],
    report: '已完成並 commit（3 個 commit）。\n\n改動：`daemon/src/group.rs` 改用 `messages.bot_id` 直接查，join 改為 LEFT JOIN；\n刪除的 bot 以 `bot_id` 尾 6 碼當顯示名。\n\n驗證：`cargo test group::` 全過。',
  },
  {
    title: '前端合併時間軸容忍未知 bot_id',
    brief: '`GroupChatPanel` 的 `kinds[msg.bot_id]` 查不到時不要當掉，退回無 kind 的中性氣泡。',
    files: ['web/src/components/GroupChatPanel.tsx'],
    report: '已完成。`kinds[...]` 改為 optional chaining，並補一個「已刪除」徽章。\n\n驗證：`npx tsc --noEmit` 與 `npm run build` 都過。',
  },
  {
    title: '補上刪除 bot 後的歷史保留測試',
    brief: '新增整合測試：建立 bot → 送訊息 → 刪除 bot → `GET /projects/:id/messages` 仍看得到那則訊息。',
    files: ['daemon/tests/group_history.rs'],
    report: '已完成，新增 `daemon/tests/group_history.rs`（2 個案例）。\n\n驗證：`cargo test --test group_history` 全過。',
  },
  {
    title: '文件：把「刪除 bot 不刪訊息」寫進 SPEC §6.4',
    brief: '補一句話說明 DELETE bot 的訊息保留語意，並在 API.md 對應段落加註。',
    files: ['docs/SPEC.md', 'docs/API.md'],
    report: '已完成，SPEC §6.4 與 API.md §10.3 各補一段。',
  },
]

/** SPEC §11.2 `[[hosts]]` + the runtime connection state the daemon reports. */
interface MockHost {
  name: string
  ssh: string
  ssh_port: number
  herdr_session: string
  remote_path: string
  connected: boolean
  error: string | null
  /** v4.0 tool detection on that host */
  tools: Record<BotKind, MockTool>
  /** v4.0 per-identity login state on that host, keyed by identity name. */
  identities: Record<string, MockIdentityStatus>
}

interface MockTool {
  installed: boolean
  path: string | null
  version: string | null
  logged_in: boolean | null
}

/** `POST /api/hosts/:name/shells` 開出來的假 shell（有真的行緩衝，見 `shellText`）。 */
interface MockShell {
  host: string
  pane_id: string
  tab_id: string
  workspace_id: string
  cwd: string
  created_at: string
  /** 已經「印出去」的行。 */
  lines: string[]
  /** 還在提示符後面、還沒按 Enter 的字。 */
  typed: string
}

interface MockGh {
  installed: boolean
  path: string
  logged_in: boolean
  account: string | null
  accounts: { login: string; active: boolean; ok: boolean }[]
  pending: {
    user_code: string
    verification_uri: string
    verification_uri_complete: string | null
    expires_in: number
    started: number
  } | null
  error: string | null
}

function mockGhLoggedIn(): MockGh {
  return {
    installed: true,
    path: '/opt/homebrew/bin/gh',
    logged_in: true,
    account: 'Eden-Sun',
    accounts: [{ login: 'Eden-Sun', active: true, ok: true }],
    pending: null,
    error: null,
  }
}

/** m4p 實測形狀：作用中 token 失效，另有一個有效但非 active 的帳號。 */
function mockGhNeedsSwitch(): MockGh {
  return {
    installed: true,
    path: '/opt/homebrew/bin/gh',
    logged_in: false,
    account: 'eddysun-alt',
    accounts: [
      { login: 'eddysun-alt', active: true, ok: false },
      { login: 'Eden-Sun', active: false, ok: true },
    ],
    pending: null,
    error: null,
  }
}

/** v4.0 `hosts[].identities.<name>` — 身份在「那一台」上的登入狀態。 */
interface MockIdentityStatus {
  name: string
  kind: BotKind
  logged_in: boolean | null
  account?: string
  plan?: string
  /** `config` = config.toml 的 `[[identities]]`；`shell` = 那台主機 zshrc 的 `ccN`（SPEC §16）。 */
  source: 'config' | 'shell'
  /** shell 來源的 `CLAUDE_CONFIG_DIR`（`cc0` 這種預設帳號沒有）。 */
  config_dir?: string
}

const TOOLS_ALL_OK: Record<BotKind, MockTool> = {
  claude: { installed: true, path: '/opt/homebrew/bin/claude', version: '2.1.40', logged_in: true },
  codex: { installed: true, path: '/opt/homebrew/bin/codex', version: '0.68.0', logged_in: true },
  grok: { installed: true, path: '/Users/me/.local/bin/grok', version: '1.0.13', logged_in: null },
}

/** v4.0 `GET /api/models` catalogue (what the CLIs report on 2026-09-06). */
/** claude 2.1 的 `--effort`：每個 alias 都是同一組五級（不像 codex 是 per-model）。 */
const CLAUDE_EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max']

const MODELS: Record<BotKind, Rec[]> = {
  claude: [
    { id: 'opus', display_name: 'Opus', description: '最強推理', is_default: false, default_effort: null, efforts: CLAUDE_EFFORTS, service_tiers: [] },
    { id: 'sonnet', display_name: 'Sonnet', description: '速度與品質平衡', is_default: true, default_effort: null, efforts: CLAUDE_EFFORTS, service_tiers: [] },
    { id: 'haiku', display_name: 'Haiku', description: '最快、最省', is_default: false, default_effort: null, efforts: CLAUDE_EFFORTS, service_tiers: [] },
    { id: 'fable', display_name: 'Fable', description: 'Fable 5.1', is_default: false, default_effort: null, efforts: CLAUDE_EFFORTS, service_tiers: [] },
  ],
  codex: [
    {
      id: 'gpt-5.5',
      display_name: 'GPT-5.5',
      description: '日常編碼的預設模型',
      is_default: true,
      default_effort: 'medium',
      efforts: ['low', 'medium', 'high', 'xhigh'],
      service_tiers: [
        { id: 'default', name: 'Standard', description: '一般佇列' },
        { id: 'priority', name: 'Fast', description: '優先佇列，較快但額度消耗較高' },
      ],
    },
    {
      id: 'gpt-5.6-luna',
      display_name: 'GPT-5.6 Luna',
      description: '長脈絡、重構友善',
      is_default: false,
      default_effort: 'high',
      efforts: ['medium', 'high', 'xhigh'],
      service_tiers: [
        { id: 'default', name: 'Standard', description: '一般佇列' },
        { id: 'priority', name: 'Fast', description: '優先佇列' },
      ],
    },
    {
      id: 'gpt-5.6-sol',
      display_name: 'GPT-5.6 Sol',
      description: '可靠的日常 agentic workhorse',
      is_default: false,
      default_effort: 'low',
      efforts: ['low', 'medium', 'high', 'xhigh', 'max', 'ultra'],
      service_tiers: [],
    },
    {
      id: 'gpt-6-astra',
      display_name: 'GPT-6 Astra',
      description: '規格審視 / 深度推理（無 fast tier）',
      is_default: false,
      default_effort: 'high',
      efforts: ['high', 'xhigh'],
      service_tiers: [{ id: 'default', name: 'Standard', description: '一般佇列' }],
    },
  ],
  grok: [
    { id: 'grok-4.6', display_name: 'Grok 4.6', description: 'grok CLI 預設', is_default: true, default_effort: 'high', efforts: ['low', 'medium', 'high', 'xhigh'], service_tiers: [] },
    { id: 'grok-4.5', display_name: 'Grok 4.5', description: '上一代', is_default: false, default_effort: 'high', efforts: ['low', 'medium', 'high'], service_tiers: [] },
  ],
}

/**
 * claude 的 `default_effort` 不是模型內建的，是那個帳號 `settings.json` 的 `effortLevel`
 * （全域）加上 `modelSettings.<真實 model id>.effortLevel`（per-model 覆寫，SPEC §17.1）。
 * 這裡的三組數字照真機實測抄過來，好讓 demo 換身份時看到的是真的會發生的情況：
 * 預設帳號／cc0 全域 high、opus 被覆寫成 low；cc1 只有全域 medium；cc2 兩者都沒設過。
 *
 * cc2 沒設過不代表沒有預設——claude 自己的內建預設是 `high`（Claude Code 官方文件
 * `code.claude.com/docs/en/model-config`：「`high`…The default on every model except
 * Opus 4.7」；2026-09-07 拿一個乾淨的 cc2 帳號實測也印出 `Sonnet 5 with high effort`）。
 * `opus`/`sonnet`/`haiku`/`fable` 沒有一個對到 Opus 4.7，所以那個例外在這裡用不到。
 */
const CLAUDE_BUILTIN_DEFAULT_EFFORT = 'high'

const CLAUDE_ACCOUNT_EFFORT: Record<string, { global?: string; overrides?: Record<string, string> }> = {
  '': { global: 'high', overrides: { opus: 'low' } },
  cc0: { global: 'high', overrides: { opus: 'low' } },
  cc1: { global: 'medium' },
  cc2: {},
}

function claudeModelsForIdentity(identity: string): Rec[] {
  const cfg = CLAUDE_ACCOUNT_EFFORT[identity] ?? CLAUDE_ACCOUNT_EFFORT['']
  return MODELS.claude.map((m) => ({
    ...m,
    default_effort: cfg.overrides?.[String(m.id)] ?? cfg.global ?? CLAUDE_BUILTIN_DEFAULT_EFFORT,
  }))
}

function inHours(h: number): string {
  return new Date(Date.now() + h * 3600_000).toISOString()
}

const REPLIES = [
  '好的，我看了一下 `src/api/transport.ts`：token 會在 `GET /api/session` 之後快取，後續 REST 都帶 `X-AM-Token`。\n\n需要我把重連的退避上限調整成 30 秒嗎？',
  '已完成。修改重點：\n\n1. `runs_one_active` 部分唯一索引避免重複啟動\n2. per-bot mutex 包住 start / stop / prompt\n3. hook 早於 RPC 回應時仍能配對 in-flight Turn\n\n測試都過了。',
  '這段的問題在於 `agent.start` 是非同步的，socket 立刻回 `launch_pending:true`，所以必須接 `agent.wait {until:[idle,done,blocked]}` 才能確定就緒。',
  'PONG',
  // A reply that is only a fenced one-liner (what `echo 1` really produced) — renders as plain mono text.
  '```\n1\n```',
]

export class MockTransport implements Transport {
  readonly mock = true

  private hosts: MockHost[] = []
  /** v4.0: the local machine's tool detection (grok missing, codex not logged in — for the hint UI). */
  private localTools: Record<BotKind, MockTool> = {
    claude: { ...TOOLS_ALL_OK.claude },
    codex: { ...TOOLS_ALL_OK.codex, logged_in: false },
    grok: { installed: false, path: null, version: null, logged_in: null },
  }
  /**
   * v4.0：身份在本機的登入狀態。cc0 有帳號、cc1 沒有——這正是遠端主機最常見的落差，
   * mock 讓身份選擇器的「未登入」標記在沒有 daemon 時也看得到。
   */
  /**
   * 本機每個身份的登入狀態。`cc2` 是從 zshrc 的 alias 認來的（SPEC §16）——config 裡沒有它，
   * 一樣可以指派給 Bot，UI 要標得出兩種來源的差別。
   */
  private localIdentityStatus: Record<string, MockIdentityStatus> = {
    cc0: { name: 'cc0', kind: 'claude', logged_in: true, account: 'me@example.com', plan: 'max', source: 'config' },
    cc1: { name: 'cc1', kind: 'claude', logged_in: false, source: 'config' },
    cc2: {
      name: 'cc2',
      kind: 'claude',
      logged_in: true,
      account: 'me+2@example.com',
      plan: 'pro',
      source: 'shell',
      config_dir: '/Users/me/.claude-cc2',
    },
  }
  /**
   * 額度按主機分（SPEC §14）：裸 key 是本機，遠端主機加 `<host>/` 前綴。
   * 新增遠端主機時 `seedHostQuota()` 會補上那台的列，數字故意和本機不同——切過去
   * 要看得出來換了一台，而不是同一組數字。
   */
  private quota: Record<string, Rec | null> = {
    claude: { five_hour: { used_pct: 18, resets_at: inHours(2.4) }, seven_day: { used_pct: 40, resets_at: inHours(70) }, plan: 'Max 20x', updated_at: now(), host: 'local' },
    'claude:cc1': { five_hour: { used_pct: 85, resets_at: inHours(1.1) }, seven_day: { used_pct: 30, resets_at: inHours(120) }, plan: 'Pro', updated_at: now(), host: 'local' },
    // zshrc 認來的身份也有自己的額度列（SPEC §16）。
    'claude:cc2': { five_hour: { used_pct: 24, resets_at: inHours(3.8) }, seven_day: { used_pct: 51, resets_at: inHours(88) }, plan: 'Pro', updated_at: now(), host: 'local' },
    codex: { five_hour: { used_pct: 63, resets_at: inHours(3.2) }, seven_day: { used_pct: 88, resets_at: inHours(41) }, plan: 'Plus', updated_at: now(), host: 'local' },
    grok: null,
  }
  private identities: MockIdentity[] = [
    { name: 'cc0', kind: 'claude', env: {}, args: [] },
    { name: 'cc1', kind: 'claude', env: { CLAUDE_CONFIG_DIR: '$HOME/.claude-ccompany' }, args: [] },
  ]
  private projects: MockProject[] = []
  private bots: MockBot[] = []
  private runs: MockRun[] = []
  private turns: MockTurn[] = []
  private messages: MockMessage[] = []
  private teams: MockTeam[] = []
  private teamTasks: MockTeamTask[] = []
  private teamEvents: MockTeamEvent[] = []
  /** team_id → 還沒跑完的假 scheduler 步驟（一步 ≈ 一次 relay / 一次狀態轉移）。 */
  private teamSteps = new Map<string, (() => void)[]>()
  private teamTimers = new Map<string, ReturnType<typeof setTimeout>>()
  private conversations = new Map<string, string>()
  /** Uploaded attachment bytes, so the mock UI can render its own thumbnails. */
  private blobs = new Map<string, Blob>()

  /** Dev helper：模擬「舊 daemon 完全沒有 team 端點」（`__amMock.teamsOff()`）。 */
  private teamsDisabled = false

  /** Dev helper：模擬「daemon 還沒有 `pane/move-to-tab`」（`__amMock.paneMoveOff()`）。 */
  private paneMoveDisabled = false

  /** `POST /api/hosts/:name/shells` 開出來的假 shell，key = `<host>/<pane_id>`。 */
  private shells = new Map<string, MockShell>()
  private shellSeq = 0

  /** Dev helper：模擬「這版 daemon 沒有主機 shell」（`__amMock.hostShellsOff()`）。 */
  private hostShellsDisabled = false

  /** `GET|POST /api/hosts/:name/gh` — 本機預設已登入；新加的遠端主機預設跟 m4p 一樣（active token 失效、另有可切帳號）。 */
  private gh = new Map<string, MockGh>()

  /**
   * herdr 預設分頁裡已經有幾個**不是 bot** 的 pane（使用者自己的 shell 之類）。demo 預設就塞得夠擠，
   * 這樣 `VITE_MOCK=1` 一開終端分頁就看得到窄 pane 警示與「移到自己的分頁」按鈕。
   * `__amMock.paneSqueeze(n)` 可以調鬆調緊。
   */
  private foreignPanes = 5

  private seq = 0
  private connected = true
  private handlers: SocketHandlers | null = null
  private socketOpen = false
  private replyIndex = 0

  constructor() {
    const p: MockProject = {
      id: ulid('proj'),
      path: '/Users/me/project/agents-manager',
      label: 'agents-manager',
      workspace_id: 'ws_demo',
      host: 'local',
      github: { owner: 'edansun', repo: 'agents-manager', url: 'https://github.com/edansun/agents-manager' },
      created_at: now(),
    }
    this.projects.push(p)
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-claude',
      kind: 'claude',
      model: null,
      effort: null,
      fast: 0,
      persona: '你是 agents-manager 的 PM。回覆用繁體中文，先給結論再列理由；改動前先說明影響範圍。',
      args_json: '[]',
      autostart: 1,
      inject_hooks: 1,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      managed_by: 'user',
      team_id: null,
      team_role: null,
      cwd: null,
      created_at: now(),
    })
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-codex',
      kind: 'codex',
      model: null,
      effort: null,
      fast: 0,
      persona: null,
      args_json: '[]',
      autostart: 0,
      inject_hooks: 1,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      managed_by: 'user',
      team_id: null,
      team_role: null,
      cwd: null,
      created_at: now(),
    })
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-grok',
      kind: 'grok',
      model: null,
      effort: null,
      fast: 0,
      persona: null,
      args_json: '[]',
      autostart: 0,
      inject_hooks: 1,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      managed_by: 'user',
      team_id: null,
      team_role: null,
      cwd: null,
      created_at: now(),
    })
    installDevHelpers(this)
  }

  // ---------------------------------------------------------------- transport

  session(): Promise<string> {
    return Promise.resolve('mock-ui-token')
  }

  /** `POST /bots/:id/attachments` — keeps the bytes in memory and hands back metadata. */
  async upload(path: string, file: Blob): Promise<unknown> {
    await this.session()
    const id = ulid('att')
    this.blobs.set(id, file)
    const name = new URLSearchParams(path.split('?')[1] ?? '').get('name') || 'image'
    return {
      id,
      name,
      mime: file.type || 'image/png',
      size: file.size,
      path: `/Users/me/project/agents-manager/.agents-manager/attachments/${id}.png`,
    }
  }

  async blobUrl(path: string): Promise<string> {
    const id = decodeURIComponent(path.replace('/attachments/', ''))
    const blob = this.blobs.get(id)
    if (!blob) throw new ApiError(404, { error: 'not_found', reason: 'unknown attachment' }, 'not found')
    return URL.createObjectURL(blob)
  }

  openSocket(handlers: SocketHandlers): () => void {
    this.handlers = handlers
    handlers.onStatus('connecting')
    setTimeout(() => {
      if (this.handlers !== handlers) return
      this.socketOpen = true
      handlers.onStatus('open')
    }, 120)
    // v4.0: codex usage creeps up every 20 s (WS `quota_updated`).
    const drift = setInterval(() => {
      if (this.handlers !== handlers) return clearInterval(drift)
      const q = this.quota.codex
      if (!q) return
      const fh = q.five_hour as Rec
      fh.used_pct = Math.min(100, Number(fh.used_pct) + 1)
      q.updated_at = now()
      this.emit('quota_updated', { kind: 'codex', host: 'local', quota: q })
    }, 20000)
    return () => {
      if (this.handlers === handlers) {
        this.handlers = null
        this.socketOpen = false
      }
    }
  }

  async request(method: HttpMethod, path: string, body?: unknown): Promise<unknown> {
    // A little latency so loading states are visible.
    await sleep(60)
    const [rawPath, query] = path.split('?')
    const q = new URLSearchParams(query ?? '')
    const b = (body ?? {}) as Rec
    const seg = rawPath.split('/').filter(Boolean)

    if (method === 'GET' && rawPath === '/state') return this.state()
    if (method === 'GET' && rawPath === '/fs/dirs') {
      return this.dirs(q.get('path') ?? '', q.get('host') ?? '', q.get('hidden') === '1')
    }
    if (method === 'GET' && rawPath === '/models') return this.models(q.get('kind') ?? '', q.get('host') ?? '', q.get('identity') ?? '')
    if (method === 'GET' && rawPath === '/quota') return { kinds: this.quota }
    if (method === 'GET' && rawPath === '/mem') return this.mem()
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'tools' && seg[3] === 'install') return this.installTool(seg[1], b)
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'tools' && seg[3] === 'refresh') return this.refreshTools(seg[1])
    if (seg[0] === 'hosts' && seg[2] === 'gh' && method === 'GET' && seg.length === 3) return this.ghStatus(seg[1])
    if (seg[0] === 'hosts' && seg[2] === 'gh' && seg[3] === 'login' && method === 'POST') return this.ghLogin(seg[1], b)
    if (seg[0] === 'hosts' && seg[2] === 'gh' && seg[3] === 'cancel' && method === 'POST') return this.ghCancel(seg[1])
    if (seg[0] === 'hosts' && seg[2] === 'shells' && !this.hostShellsDisabled) {
      const host = decodeURIComponent(seg[1])
      const pane = seg[3] ? decodeURIComponent(seg[3]) : ''
      if (method === 'POST' && !pane) return this.openShell(host, String(b.cwd ?? ''))
      if (method === 'GET' && !pane) return { host, shells: this.shellsOf(host), max: 8 }
      if (method === 'DELETE' && pane && seg.length === 4) return this.closeShell(host, pane)
      if (method === 'GET' && seg[4] === 'terminal') {
        return this.shellTerminal(host, pane, q.get('source') ?? 'visible', Number(q.get('lines') ?? 200))
      }
      if (method === 'POST' && seg[4] === 'text') return this.shellText(host, pane, String(b.text ?? ''), b.enter !== false)
      if (method === 'POST' && seg[4] === 'keys') return this.shellKeys(host, pane, b.keys)
    }

    if (method === 'POST' && rawPath === '/identities') return this.addIdentity(b)
    if (seg[0] === 'identities' && seg.length === 2 && method === 'DELETE') return this.deleteIdentity(decodeURIComponent(seg[1]))
    if (method === 'POST' && rawPath === '/hosts') return this.addHost(b)
    if (seg[0] === 'hosts' && seg.length === 2 && method === 'DELETE') return this.deleteHost(seg[1])
    if (seg[0] === 'hosts' && seg[2] === 'reconnect' && method === 'POST') return this.reconnectHost(seg[1])

    if (method === 'POST' && rawPath === '/projects') return this.addProject(b)
    if (method === 'DELETE' && seg[0] === 'projects' && seg.length === 2) return this.deleteProject(seg[1])
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'bots') return this.addBot(seg[1], b)
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'messages') return this.projectMessages(seg[1], q)
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'submodules') return { project_id: seg[1], submodules: [] }
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'issues') return this.issues(seg[1], seg[3], q)
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'chat') return this.projectChat(seg[1], b)
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'teams' && !this.teamsDisabled) {
      return this.createTeam(seg[1], b)
    }

    // ---- SPEC-team §10 -------------------------------------------------
    if (seg[0] === 'teams' && seg.length >= 2 && !this.teamsDisabled) {
      const teamId = seg[1]
      if (method === 'GET' && seg.length === 2) return this.teamDetail(teamId)
      if (method === 'GET' && seg[2] === 'events') return { team_id: teamId, events: this.teamEventsOf(teamId) }
      if (method === 'PATCH' && seg.length === 2) return this.patchTeam(teamId, b)
      if (method === 'POST' && seg[2] === 'issues' && seg.length === 3) return this.addTeamIssues(teamId, b)
      if (method === 'POST' && seg[2] === 'tasks' && seg[4] === 'decide') return this.decideTask(teamId, seg[3], b)
      if (method === 'POST' && seg[2] === 'say') return this.teamSay(teamId, b)
      if (method === 'POST' && seg[2] === 'answer') return this.teamAnswer(teamId, b)
      if (method === 'POST' && seg[2] === 'pause') return this.pauseTeam(teamId)
      if (method === 'POST' && seg[2] === 'resume') return this.resumeTeam(teamId)
      if (method === 'POST' && seg[2] === 'approve') return this.approveTeam(teamId)
      if (method === 'POST' && seg[2] === 'abort') return this.abortTeam(teamId)
      if (method === 'POST' && seg[2] === 'cleanup') return this.cleanupTeam(teamId)
      if (method === 'POST' && seg[2] === 'close-issue') return this.closeTeamIssue(teamId, b)
      if (method === 'DELETE' && seg.length === 2) {
        return this.deleteTeam(teamId, q.get('branches') === 'delete' ? 'delete' : 'keep')
      }
    }

    if (seg[0] === 'bots' && seg.length >= 2) {
      const botId = seg[1]
      const action = seg[2]
      if (method === 'PATCH' && !action) return this.patchBot(botId, b)
      if (method === 'DELETE' && !action) return this.deleteBot(botId)
      if (method === 'GET' && action === 'messages') return this.messagesOf(botId)
      if (method === 'GET' && action === 'terminal') {
        return this.terminal(botId, q.get('source') ?? 'visible', Number(q.get('lines') ?? 40))
      }
      if (method === 'POST') {
        if (action === 'start') return this.start(botId)
        if (action === 'stop') return this.stop(botId)
        if (action === 'restart') return this.restart(botId)
        if (action === 'interrupt') return this.interrupt(botId)
    if (action === 'abort') return this.abort(botId)
        if (action === 'login') return this.login(botId)
        if (action === 'prompt') return this.prompt(botId, b)
        if (action === 'keys') return this.keys(botId, b)
        if (action === 'text') return this.text(botId, b)
        if (action === 'pane' && seg[3] === 'move-to-tab' && !this.paneMoveDisabled) return this.movePaneToTab(botId)
      }
    }

    if (method === 'POST' && seg[0] === 'turns' && seg[2] === 'abandon') return this.abandon(seg[1])

    throw new ApiError(404, { reason: `mock: no route for ${method} ${rawPath}` }, 'not found')
  }

  // ------------------------------------------------------------------ helpers

  /** v4.0 `GET /api/projects/:id/issues[/:number]` — fake `gh`; `?q=gh-error` simulates a 502. */
  private issues(projectId: string, number: string | undefined, q: URLSearchParams) {
    const p = this.projects.find((x) => x.id === projectId)
    if (!p) throw new ApiError(404, { error: 'not_found', what: 'project' }, 'not found')
    if (!p.github) throw new ApiError(400, { error: 'bad_request', message: 'project has no GitHub remote' }, 'bad request')
    const list: Rec[] = ISSUES.map((i) => ({ ...i, url: `${p.github?.url}/issues/${String(i.number)}` }))
    if (number) {
      const one = list.find((i) => String(i.number) === number)
      if (!one) throw new ApiError(404, { error: 'not_found', what: 'issue' }, 'not found')
      return { issue: one }
    }
    const query = (q.get('q') ?? '').trim().toLowerCase()
    if (query === 'gh-error') {
      throw new ApiError(502, { error: 'upstream', message: 'gh: To get started with GitHub CLI, please run: gh auth login' }, 'upstream')
    }
    const state = q.get('state') ?? 'open'
    const limit = Number(q.get('limit') ?? 30) || 30
    return {
      project_id: projectId,
      github: p.github,
      issues: list
        .filter((i) => (state === 'all' ? true : i.state === state))
        .filter((i) => !query || `#${String(i.number)} ${String(i.title)}`.toLowerCase().includes(query))
        .slice(0, limit)
        .map((i) => ({ ...i, body: undefined, body_excerpt: String(i.body).split('\n')[0].slice(0, 120) })),
    }
  }

  /** v4.0 `GET /api/models?kind=&host=&identity=` — unknown kind → 400; a down host → 502. */
  private models(kind: string, host: string, identity: string) {
    if (!BOT_KINDS.includes(kind as BotKind)) {
      throw new ApiError(400, { error: 'bad_request', message: 'kind must be claude, codex or grok' }, 'bad request')
    }
    const remote = host && host !== 'local' ? this.host(host) : null
    if (remote && !remote.connected) {
      throw new ApiError(502, { error: 'upstream', message: `主機 ${remote.name} 未連線` }, 'upstream')
    }
    const models = kind === 'claude' ? claudeModelsForIdentity(identity) : MODELS[kind as BotKind]
    return { kind, host: remote?.name ?? 'local', models }
  }

  /**
   * v4.0 `POST /api/hosts/:name/tools/install {kind, via_bot_id}`: the install + login
   * instructions go to a running bot on that host as an ordinary prompt.
   */
  private installTool(hostName: string, b: Rec) {
    const kind = String(b.kind ?? '')
    if (!BOT_KINDS.includes(kind as BotKind)) {
      throw new ApiError(400, { error: 'bad_request', message: 'kind must be claude, codex or grok' }, 'bad request')
    }
    const remote = hostName && hostName !== 'local' ? this.host(hostName) : null
    const botId = String(b.via_bot_id ?? '')
    const bot = this.bots.find((x) => x.id === botId)
    const project = bot ? this.projects.find((p) => p.id === bot.project_id) : undefined
    if (!bot || !project || project.host !== (remote?.name ?? 'local')) {
      throw new ApiError(404, { error: 'not_found', what: 'via_bot_id (on that host)' }, 'not found')
    }
    if (!this.activeRun(botId)) {
      throw new ApiError(409, { error: 'conflict', reason: 'via bot is not running' }, 'conflict')
    }
    const text =
      `請在這台主機安裝並登入 ${kind} CLI：\n` +
      `1. 安裝（brew / npm / 官方安裝腳本擇一）\n2. 執行 \`${kind} login\` 完成登入（會顯示 URL，請貼給我）\n3. 回報 \`${kind} --version\``
    const reply =
      `已安裝 ${kind}（mock）。\n\n` +
      '```\n' + `$ ${kind} --version\n${kind} 1.0.13\n` + '```\n\n' +
      `登入請開啟 https://example.invalid/login?device=MOCK-${kind.toUpperCase()}`
    const res = this.prompt(botId, { text, client_request_id: ulid('cr') }, null, reply)
    const tools = remote ? remote.tools : this.localTools
    setTimeout(() => {
      tools[kind as BotKind] = { installed: true, path: `/usr/local/bin/${kind}`, version: '1.0.13', logged_in: true }
      this.emit('host_changed', { name: remote?.name ?? 'local', connected: true, error: null, tools })
    }, 3500)
    return { turn_id: res.turn_id }
  }

  /**
   * SPEC §15：herdr 進程樹的常駐記憶體。mock 用「每個執行中的 bot 各吃一份」推出來，
   * 這樣啟動 / 停止 bot 時上面那格真的會動，看得出它連著什麼。
   */
  private mem() {
    const HERDR = 48 * 1024 * 1024
    const PER_BOT: Record<string, number> = { claude: 820, codex: 640, grok: 410 }
    const hosts = [{ name: 'local' as string, connected: true }, ...this.hosts.map((h) => ({ name: h.name, connected: h.connected }))]
    const rows = hosts.map((h) => {
      if (!h.connected) {
        return { host: h.name, herdr_bytes: 0, agents_bytes: 0, total_bytes: 0, processes: 0, error: '未連線' }
      }
      const pids = new Set(this.projects.filter((p) => p.host === h.name).map((p) => p.id))
      const live = this.bots.filter((b) => pids.has(b.project_id) && this.activeRun(b.id))
      const agents = live.reduce((n, b) => n + (PER_BOT[b.kind] ?? 500) * 1024 * 1024, 0)
      // herdr + 一個 shell + 每個 bot 一個 CLI
      const processes = live.length === 0 ? 1 : 1 + live.length * 2
      return {
        host: h.name,
        herdr_bytes: HERDR,
        agents_bytes: agents,
        total_bytes: HERDR + agents,
        processes,
        error: null,
      }
    })
    return {
      total_bytes: rows.reduce((n, r) => n + r.total_bytes, 0),
      herdr_bytes: rows.reduce((n, r) => n + r.herdr_bytes, 0),
      agents_bytes: rows.reduce((n, r) => n + r.agents_bytes, 0),
      processes: rows.reduce((n, r) => n + r.processes, 0),
      hosts: rows,
    }
  }

  /** SPEC §11.5: the same JSON shape for local and remote; `host` picks the tree. */
  private dirs(path: string, host: string, hidden = false) {
    const remote = host && host !== 'local' ? this.host(host) : null
    if (remote && !remote.connected) {
      throw new ApiError(502, { error: 'upstream', message: `主機 ${remote.name} 未連線：${remote.error ?? 'ssh 中斷'}` }, 'upstream')
    }
    const user = remote ? (remote.ssh.split('@')[0] || remote.name) : 'me'
    const home = `/Users/${user}`
    const tree: Record<string, string[]> = remote
      ? {
          '/': ['Users', 'opt', 'tmp'],
          '/Users': [user],
          [home]: ['work', 'src', 'Documents', '.config'],
          [`${home}/.config`]: [],
          [`${home}/work`]: ['api-server', 'web-client', 'scratch'],
          [`${home}/work/api-server`]: ['crates'],
          [`${home}/src`]: ['herdr'],
          [`${home}/Documents`]: [],
        }
      : {
          '/': ['Users', 'opt', 'tmp'],
          '/Users': ['me'],
          '/Users/me': ['project', 'Documents', 'Downloads', '.claude', '.config'],
          '/Users/me/.claude': ['projects'],
          '/Users/me/.config': [],
          '/Users/me/project': ['foo', 'bar', 'agents-manager'],
          '/Users/me/project/foo': ['src'],
          '/Users/me/Documents': [],
          // A long level, so the picker's scrolling and filtering can actually be exercised.
          '/Users/me/Downloads': Array.from({ length: 24 }, (_, i) => `dl-${String(i + 1).padStart(2, '0')}`),
        }
    const gitDirs = remote
      ? new Set([`${home}/work/api-server`, `${home}/work/web-client`, `${home}/src/herdr`])
      : new Set(['/Users/me/project/foo', '/Users/me/project/agents-manager'])
    const cur = path && (path in tree || path.startsWith(`${home}/`)) ? path : home
    const kids = (tree[cur] ?? []).filter((n) => hidden || !n.startsWith('.'))
    const parent = cur === '/' ? null : cur.slice(0, cur.lastIndexOf('/')) || '/'
    return {
      path: cur,
      parent,
      home,
      entries: kids.map((name) => {
        const full = cur === '/' ? `/${name}` : `${cur}/${name}`
        return { name, path: full, git: gitDirs.has(full) }
      }),
    }
  }

  // -------------------------------------------------------------- identities

  private addIdentity(b: Rec) {
    const name = String(b.name ?? '').trim()
    if (!/^[a-z][a-z0-9_-]{0,31}$/.test(name)) {
      throw new ApiError(400, { error: 'bad_request', message: 'name 必須符合 [a-z][a-z0-9_-]{0,31}' }, 'bad request')
    }
    if (this.identities.some((x) => x.name === name)) {
      throw new ApiError(409, { error: 'conflict', reason: `identity 已存在：${name}` }, 'conflict')
    }
    const env: Record<string, string> = {}
    if (b.env && typeof b.env === 'object') for (const [k, v] of Object.entries(b.env as Rec)) env[k] = String(v)
    this.identities.push({
      name,
      kind: toKind(b.kind),
      env,
      args: Array.isArray(b.args) ? b.args.map(String) : [],
    })
    // 新身份在每一台上都還沒偵測過：daemon 會補跑一次偵測，mock 直接給「未知」。
    this.localIdentityStatus[name] = { name, kind: toKind(b.kind), logged_in: null, source: 'config' }
    for (const h of this.hosts) h.identities[name] = { name, kind: toKind(b.kind), logged_in: null, source: 'config' }
    this.emit('identities_changed', {})
    return {}
  }

  private deleteIdentity(name: string) {
    const used = this.bots.filter((x) => x.identity === name)
    if (used.length > 0) {
      throw new ApiError(409, { error: 'conflict', reason: `仍有 ${used.length} 個 Bot 使用身份 ${name}` }, 'conflict')
    }
    this.identities = this.identities.filter((x) => x.name !== name)
    delete this.localIdentityStatus[name]
    for (const h of this.hosts) delete h.identities[name]
    this.emit('identities_changed', {})
    return {}
  }

  /**
   * v4.0 `POST /api/hosts/:name/tools/refresh` — 重跑 CLI + 每個身份的登入偵測。
   * mock 不會憑空生出帳號，所以答案和上一次一樣；重點是端點形狀與 busy 狀態。
   */
  private refreshTools(host: string) {
    const remote = host && host !== 'local' ? this.host(host) : null
    const tools = remote ? remote.tools : this.localTools
    const identities = remote ? remote.identities : this.localIdentityStatus
    return { name: remote?.name ?? 'local', tools, identities, tools_checked_at: now() }
  }

  private ghKey(name: string): string {
    const n = decodeURIComponent(name || 'local')
    if (n === 'local' || !n) return 'local'
    this.host(n)
    return n
  }

  private ghOf(name: string): MockGh {
    const key = this.ghKey(name)
    let g = this.gh.get(key)
    if (!g) {
      g = key === 'local' ? mockGhLoggedIn() : mockGhNeedsSwitch()
      this.gh.set(key, g)
    }
    if (g.pending && Date.now() - g.pending.started > 1600) {
      Object.assign(g, mockGhLoggedIn(), { pending: null, error: null })
    }
    return g
  }

  private ghJson(name: string, mode: string | null) {
    const key = this.ghKey(name)
    const g = this.ghOf(key)
    const pending = g.pending
      ? {
          user_code: g.pending.user_code,
          verification_uri: g.pending.verification_uri,
          verification_uri_complete: g.pending.verification_uri_complete,
          expires_in: Math.max(0, g.pending.expires_in - Math.floor((Date.now() - g.pending.started) / 1000)),
        }
      : null
    return {
      name: key,
      installed: g.installed,
      path: g.path,
      logged_in: g.logged_in,
      account: g.account,
      accounts: g.accounts,
      mode,
      pending,
      error: g.error,
    }
  }

  private ghStatus(name: string) {
    return this.ghJson(name, null)
  }

  private ghLogin(name: string, b: Rec) {
    const key = this.ghKey(name)
    const mode = String(b.mode ?? 'auto').trim() || 'auto'
    if (!['auto', 'copy', 'device', 'switch'].includes(mode)) {
      throw new ApiError(400, { error: 'bad_request', message: `mode must be auto, copy, device or switch (got ${mode})` }, 'bad request')
    }
    const g = this.ghOf(key)
    if (!g.installed) throw new ApiError(502, { error: 'upstream', message: 'gh 未安裝（brew install gh）' }, 'upstream')
    if (mode === 'copy' && key === 'local') {
      throw new ApiError(400, { error: 'bad_request', message: 'copy 只適用遠端主機（本機請用裝置碼）' }, 'bad request')
    }
    const finish = (used: string) => {
      Object.assign(g, mockGhLoggedIn(), { pending: null, error: null })
      return this.ghJson(key, used)
    }
    if (mode === 'auto' && g.logged_in) return this.ghJson(key, 'auto')
    if ((mode === 'auto' || mode === 'switch') && g.accounts.some((a) => a.ok && !a.active)) return finish('switch')
    if ((mode === 'auto' && key !== 'local') || mode === 'copy') {
      if (!this.ghOf('local').logged_in) {
        throw new ApiError(409, { error: 'conflict', reason: 'local_gh_not_logged_in', message: '本機 gh 尚未登入，無法轉發 token；改用裝置碼' }, 'conflict')
      }
      return finish('copy')
    }
    if (mode === 'switch') {
      throw new ApiError(400, { error: 'bad_request', message: '沒有可切換的有效 gh 帳號' }, 'bad request')
    }
    g.pending = {
      user_code: 'WDJB-MJHT',
      verification_uri: 'https://github.com/login/device',
      verification_uri_complete: 'https://github.com/login/device?user_code=WDJB-MJHT',
      expires_in: 900,
      started: Date.now(),
    }
    g.error = null
    return this.ghJson(key, 'device')
  }

  private ghCancel(name: string) {
    const key = this.ghKey(name)
    const g = this.ghOf(key)
    g.pending = null
    g.error = null
    return this.ghJson(key, 'cancel')
  }

  // -------------------------------------------------------------- hosts (§11.6)

  private host(name: string): MockHost {
    const h = this.hosts.find((x) => x.name === name)
    if (!h) throw new ApiError(404, { error: 'not_found', what: 'host' }, 'host not found')
    return h
  }

  private hostMap(): Record<string, { connected: boolean; error: string | null }> {
    const out: Record<string, { connected: boolean; error: string | null }> = {
      local: { connected: this.connected, error: null },
    }
    for (const h of this.hosts) out[h.name] = { connected: h.connected, error: h.error }
    return out
  }

  /** Fake ssh dial: anything with `fail` / `bad` / an unreachable-looking target stays down. */
  private dial(h: MockHost) {
    const bad = /fail|bad|unreachable|0\.0\.0\.0/i.test(h.ssh)
    h.connected = !bad
    h.error = bad ? `ssh: connect to host ${h.ssh.split('@').pop()} port ${h.ssh_port}: Operation timed out` : null
  }

  private async addHost(b: Rec) {
    const name = String(b.name ?? '').trim()
    if (!/^[a-z][a-z0-9_-]{0,31}$/.test(name)) {
      throw new ApiError(400, { error: 'bad_request', message: 'name 必須符合 [a-z][a-z0-9_-]{0,31}' }, 'bad request')
    }
    if (name === 'local') {
      throw new ApiError(400, { error: 'bad_request', message: '`local` 為保留名稱' }, 'bad request')
    }
    const ssh = String(b.ssh ?? '').trim()
    if (!ssh) throw new ApiError(400, { error: 'bad_request', message: 'ssh 目標不可為空' }, 'bad request')
    const h: MockHost = {
      name,
      ssh,
      ssh_port: Number(b.ssh_port ?? 22) || 22,
      herdr_session: String(b.herdr_session ?? '') || 'agents-manager',
      remote_path: String(b.remote_path ?? ''),
      connected: false,
      error: null,
      // A fresh remote box: claude + codex present, grok missing (exercises the tools hint).
      tools: { claude: { ...TOOLS_ALL_OK.claude }, codex: { ...TOOLS_ALL_OK.codex }, grok: { installed: false, path: null, version: null, logged_in: null } },
      // 遠端新機器：預設身份登得進去，另一組 `CLAUDE_CONFIG_DIR` 還沒登入。
      identities: {
        ...Object.fromEntries(
          this.identities.map((i) => [
            i.name,
            Object.keys(i.env).length === 0
              ? { name: i.name, kind: i.kind, logged_in: true, account: `${name}@example.com`, source: 'config' as const }
              : { name: i.name, kind: i.kind, logged_in: false, source: 'config' as const },
          ]),
        ),
        // 那台自己 zshrc 裡的 `ccN`：同一個名字，指到的卻是那台的目錄（SPEC §16）。
        cc2: {
          name: 'cc2',
          kind: 'claude' as BotKind,
          logged_in: true,
          account: `ops@${name}.example.com`,
          source: 'shell' as const,
          config_dir: `/Users/${name}/.claude-ccompany`,
        },
      },
    }
    // API.md: an existing name is an update (disconnect, then reconnect with the new config).
    this.hosts = this.hosts.filter((x) => x.name !== name)
    this.hosts.push(h)
    await sleep(700) // ssh master + remote `herdr session list` take a moment
    this.dial(h)
    this.seedHostQuota(h)
    this.emit('host_changed', { name: h.name, connected: h.connected, error: h.error })
    this.emitMem()
    return { name: h.name, connected: h.connected, error: h.error }
  }

  /** 那台主機上的 daemon 輪詢會回報的額度（連上才有；斷線的主機留空）。 */
  private seedHostQuota(h: MockHost) {
    if (!h.connected) return
    const rows: Record<string, Rec | null> = {
      [`${h.name}/claude`]: { five_hour: { used_pct: 46, resets_at: inHours(1.6) }, seven_day: { used_pct: 12, resets_at: inHours(96) }, plan: 'Max 5x', updated_at: now(), host: h.name },
      [`${h.name}/claude:cc1`]: { five_hour: { used_pct: 7, resets_at: inHours(4.1) }, seven_day: { used_pct: 71, resets_at: inHours(58) }, plan: 'Pro', updated_at: now(), host: h.name },
      [`${h.name}/claude:cc2`]: { five_hour: { used_pct: 62, resets_at: inHours(2.2) }, seven_day: { used_pct: 18, resets_at: inHours(101) }, plan: 'Max 5x', updated_at: now(), host: h.name },
      [`${h.name}/codex`]: { five_hour: { used_pct: 96, resets_at: inHours(0.7) }, seven_day: { used_pct: 33, resets_at: inHours(120) }, plan: 'Plus', updated_at: now(), host: h.name },
      [`${h.name}/grok`]: null,
    }
    Object.assign(this.quota, rows)
    for (const [kind, quota] of Object.entries(rows)) {
      this.emit('quota_updated', { kind, host: h.name, quota })
    }
  }

  private deleteHost(name: string) {
    const h = this.host(name)
    const used = this.projects.filter((p) => p.host === h.name)
    if (used.length > 0) {
      throw new ApiError(
        409,
        {
          error: 'conflict',
          reason: `host still used by projects（仍有 ${used.length} 個 Project 使用 ${h.name}）`,
          project_id: used[0].id,
        },
        'conflict',
      )
    }
    this.hosts = this.hosts.filter((x) => x.name !== name)
    // daemon 也是這樣做的：主機沒了，它的額度列不留在 map 裡（SPEC §14）。
    for (const k of Object.keys(this.quota)) {
      if (k.startsWith(`${name}/`)) delete this.quota[k]
    }
    this.emit('host_changed', { name, connected: false, error: 'removed' })
    this.emit('project_changed', {})
    return {}
  }

  private async reconnectHost(name: string) {
    const h = this.host(name)
    await sleep(600)
    this.dial(h)
    this.seedHostQuota(h)
    this.emit('host_changed', { name: h.name, connected: h.connected, error: h.error })
    for (const b of this.botsOnHost(h.name)) this.emitBotStatus(b.id)
    return { name: h.name, connected: h.connected, error: h.error }
  }

  private botsOnHost(name: string): MockBot[] {
    const pids = new Set(this.projects.filter((p) => p.host === name).map((p) => p.id))
    return this.bots.filter((b) => pids.has(b.project_id))
  }

  /** A host going up or down changes what can be sampled, so the total changes with it. */
  private emitMem() {
    this.emit('mem_updated', this.mem())
  }

  /** Dev helper: flip a host up / down the way the daemon's health check would. */
  setHostConnected(name: string, connected: boolean) {
    const h = this.hosts.find((x) => x.name === name)
    if (!h) return
    h.connected = connected
    h.error = connected ? null : 'ssh master 已退出（mock 模擬斷線）'
    this.emit('host_changed', { name: h.name, connected: h.connected, error: h.error })
    this.emit('daemon_status', { herdr_connected: this.connected, connected: this.connected, default_connected: false, hosts: this.hostMap() })
    for (const b of this.botsOnHost(h.name)) this.emitBotStatus(b.id)
    this.emitMem()
  }

  hostNames(): string[] {
    return this.hosts.map((h) => h.name)
  }

  private emit(type: string, data: unknown) {
    this.seq += 1
    if (this.handlers && this.socketOpen) this.handlers.onFrame({ seq: this.seq, type, data })
  }

  private bot(id: string): MockBot {
    const bot = this.bots.find((x) => x.id === id)
    if (!bot) throw new ApiError(404, { reason: 'bot not found' }, 'bot not found')
    return bot
  }

  private activeRun(botId: string): MockRun | undefined {
    return this.runs.find(
      (r) => r.bot_id === botId && (r.state === 'starting' || r.state === 'running' || r.state === 'stopping'),
    )
  }

  /** 共用預設分頁的 pane 平分 `WORKSPACE_COLUMNS`，自己一個分頁的獨佔全寬。 */
  private paneColumns(run: MockRun): number {
    if (run.own_tab) return WORKSPACE_COLUMNS
    const shared =
      this.foreignPanes +
      this.runs.filter((r) => !r.own_tab && r.workspace_id === run.workspace_id && this.activeRun(r.bot_id) === r).length
    return Math.max(8, Math.floor(WORKSPACE_COLUMNS / Math.max(1, shared)))
  }

  /**
   * `POST /bots/:id/pane/move-to-tab` — 對應 herdr 的 `pane.move` +
   * `destination.type = "new_tab"`。搬的是**既有** pane：`pane_id` 不變，run 也不重開。
   */
  private movePaneToTab(botId: string) {
    const run = this.activeRun(botId)
    if (!run) throw new ApiError(409, { error: 'conflict', reason: '這個 bot 沒有在跑的 pane' }, 'conflict')
    run.own_tab = true
    this.emitBotStatus(botId)
    return {}
  }

  private conv(botId: string): string {
    let c = this.conversations.get(botId)
    if (!c) {
      c = ulid('conv')
      this.conversations.set(botId, c)
    }
    return c
  }

  private emitBotStatus(botId: string) {
    this.emit('bot_status', { bot_id: botId, run: this.activeRun(botId) ?? null, connected: this.connected })
    // The real poller samples on a timer; here the only thing that moves the number is a
    // run starting or stopping, so ride that instead of burning a `setInterval`.
    this.emit('mem_updated', this.mem())
  }

  private addMessage(
    m: Omit<MockMessage, 'id' | 'created_at' | 'group_id' | 'attachments_json' | 'team_id' | 'relay_from'> & {
      group_id?: string | null
      attachments_json?: string | null
      team_id?: string | null
      relay_from?: string | null
    },
  ): MockMessage {
    const msg: MockMessage = {
      group_id: null,
      attachments_json: null,
      team_id: null,
      relay_from: null,
      ...m,
      id: ulid('msg'),
      created_at: now(),
    }
    this.messages.push(msg)
    this.emit('message_added', { bot_id: msg.bot_id, message: msg })
    return msg
  }

  private updateTurn(turn: MockTurn, patch: Partial<MockTurn>) {
    Object.assign(turn, patch)
    this.emit('turn_updated', { bot_id: turn.bot_id, turn })
  }

  // ------------------------------------------------------------------- routes

  /** Mirrors `daemon/src/api.rs::state_json` exactly. */
  private state() {
    return {
      daemon_seq: this.seq,
      connected: this.connected,
      default_connected: false,
      herdr_session: 'agents-manager',
      identities: this.identities.map((i) => ({ ...i, env: { ...i.env }, args: [...i.args] })),
      // API.md: the reserved `local` entry is always first, with null ssh fields.
      hosts: [
        {
          name: 'local',
          ssh: null,
          ssh_port: null,
          herdr_session: 'agents-manager',
          remote_path: null,
          connected: this.connected,
          error: null,
          attach_command: 'herdr --session agents-manager',
          tools: this.localTools,
          identities: this.localIdentityStatus,
        },
        ...this.hosts.map((h) => ({
          name: h.name,
          ssh: h.ssh,
          ssh_port: h.ssh_port,
          herdr_session: h.herdr_session,
          remote_path: h.remote_path,
          connected: h.connected,
          error: h.error,
          attach_command: `herdr --remote ${h.ssh}${h.ssh_port !== 22 ? ` -p ${h.ssh_port}` : ''} --session ${h.herdr_session}`,
          tools: h.tools,
          identities: h.identities,
        })),
      ],
      projects: this.projects.map((p) => ({
        id: p.id,
        path: p.path,
        label: p.label,
        workspace_id: p.workspace_id,
        host: p.host,
        github: p.github,
        bots: this.bots
          .filter((b) => b.project_id === p.id)
          .map((b) => {
            const run = this.activeRun(b.id) ?? null
            return {
              id: b.id,
              project_id: b.project_id,
              name: b.name,
              kind: b.kind,
              model: b.model,
              effort: b.effort,
              fast: b.fast === 1,
              persona: b.persona,
              args: JSON.parse(b.args_json) as string[],
              autostart: b.autostart === 1,
              inject_hooks: b.inject_hooks === 1,
              auto_approve: b.auto_approve === 1,
              identity: b.identity,
              env: JSON.parse(b.env_json) as Record<string, string>,
              managed_by: b.managed_by,
              team: b.team_id && b.team_role ? { team_id: b.team_id, role: b.team_role } : null,
              cwd: b.cwd,
              // daemon 是 `slug(project.label)-<bot id 末 6 碼小寫>`，mock 照抄夠用來驗 UI。
              agent_name: `${p.label.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '') || 'b'}-${b.id.slice(-6).toLowerCase()}`,
              run,
              in_flight_turn: this.turns.find((t) => run && t.run_id === run.id && t.status === 'in_flight') ?? null,
              unread: 0,
            }
          }),
        // SPEC-team §10.2；`teamsDisabled` 時整個欄位不存在（模擬舊 daemon）。
        ...(this.teamsDisabled ? {} : { teams: this.teams.filter((t) => t.project_id === p.id).map((t) => this.teamJson(t)) }),
      })),
    }
  }

  private addProject(b: Rec) {
    const path = String(b.path ?? '').trim()
    if (!path) throw new ApiError(400, { reason: 'path 不可為空' }, 'bad request')
    const canonical = path.replace(/\/+$/, '')
    if (this.projects.some((p) => p.path === canonical)) {
      throw new ApiError(409, { reason: `專案路徑已存在：${canonical}` }, 'conflict')
    }
    const host = String(b.host ?? '').trim() || 'local'
    if (host !== 'local' && !this.hosts.some((h) => h.name === host)) {
      throw new ApiError(404, { error: 'not_found', what: 'host' }, 'host not found')
    }
    const p: MockProject = {
      id: ulid('proj'),
      path: canonical,
      label: String(b.label ?? '').trim() || (canonical.split('/').pop() ?? canonical),
      workspace_id: null,
      host,
      // Anything that looks like a repo directory name gets a fake GitHub remote.
      github: /-/.test(canonical.split('/').pop() ?? '') ? { owner: 'me', repo: canonical.split('/').pop() ?? 'repo', url: `https://github.com/me/${canonical.split('/').pop() ?? 'repo'}` } : null,
      created_at: now(),
    }
    this.projects.push(p)
    this.emit('project_changed', { project_id: p.id })
    return { project_id: p.id }
  }

  private deleteProject(id: string) {
    const bots = this.bots.filter((x) => x.project_id === id)
    if (bots.some((x) => this.activeRun(x.id))) {
      throw new ApiError(409, { reason: '仍有 Bot 在執行中，請先停止' }, 'conflict')
    }
    this.projects = this.projects.filter((p) => p.id !== id)
    this.bots = this.bots.filter((x) => x.project_id !== id)
    this.emit('project_changed', { project_id: id, deleted: true })
    return null
  }

  private addBot(projectId: string, b: Rec) {
    const name = String(b.name ?? '').trim()
    if (!/^[^\s@,:;]{1,32}$/.test(name)) {
      throw new ApiError(400, { reason: 'name：1–32 個字，不可含空白或 @ , : ;' }, 'bad request')
    }
    if (this.bots.some((x) => x.name === name)) {
      throw new ApiError(409, { reason: `agent name 已被使用：${name}` }, 'conflict')
    }
    if (!this.projects.some((p) => p.id === projectId)) {
      throw new ApiError(404, { reason: 'project not found' }, 'not found')
    }
    const bot: MockBot = {
      id: ulid('bot'),
      project_id: projectId,
      name,
      kind: toKind(b.kind),
      model: typeof b.model === 'string' && b.model.trim() ? b.model.trim() : null,
      effort: typeof b.effort === 'string' && b.effort.trim() ? b.effort.trim() : null,
      fast: b.fast === true ? 1 : 0,
      persona: typeof b.persona === 'string' && b.persona.trim() ? b.persona.trim() : null,
      args_json: JSON.stringify(Array.isArray(b.args) ? b.args : []),
      autostart: b.autostart ? 1 : 0,
      inject_hooks: 1,
      auto_approve: b.auto_approve === false ? 0 : 1,
      identity: typeof b.identity === 'string' && b.identity.trim() ? b.identity : null,
      env_json: JSON.stringify(b.env && typeof b.env === 'object' ? b.env : {}),
      managed_by: 'user',
      team_id: null,
      team_role: null,
      cwd: typeof b.cwd === 'string' && b.cwd.trim() ? b.cwd.trim() : null,
      created_at: now(),
    }
    if (bot.identity) this.checkIdentity(bot.identity, bot.kind)
    this.bots.push(bot)
    this.emit('bot_changed', { bot_id: bot.id })
    return { bot_id: bot.id }
  }

  /** identity 必須存在且 kind 相符（API.md identities 章節）。 */
  private checkIdentity(name: string, kind: BotKind) {
    const ident = this.identities.find((x) => x.name === name)
    if (!ident) throw new ApiError(404, { error: 'not_found', what: 'identity' }, 'identity not found')
    if (ident.kind !== kind) {
      throw new ApiError(
        400,
        { error: 'bad_request', message: `identity ${ident.name} 是 ${ident.kind}，bot 是 ${kind}` },
        'bad request',
      )
    }
  }

  /**
   * `PATCH /api/bots/:id`（API.md v3.3）。改名時有 active Run → 409；其他欄位允許，
   * 但回 `needs_restart: true`（目前的 Run 仍跑在舊參數上）。
   */
  private patchBot(id: string, b: Rec) {
    const bot = this.bot(id)
    const run = this.activeRun(id)
    if (b.name !== undefined && run) {
      throw new ApiError(
        409,
        { error: 'conflict', reason: 'cannot rename a bot with an active run', run_id: run.id, bot_id: id },
        'conflict',
      )
    }
    if (typeof b.name === 'string') {
      const name = b.name.trim()
      if (!/^[^\s@,:;]{1,32}$/.test(name)) {
        throw new ApiError(400, { error: 'bad_request', message: 'name：1–32 個字，不可含空白或 @ , : ;' }, 'bad request')
      }
      if (this.bots.some((x) => x.id !== id && x.name === name)) {
        throw new ApiError(409, { error: 'conflict', reason: 'bot name already in use', name }, 'conflict')
      }
      bot.name = name
    }
    if (b.identity !== undefined) {
      const name = typeof b.identity === 'string' && b.identity.trim() ? b.identity.trim() : null
      if (name) this.checkIdentity(name, bot.kind)
      bot.identity = name
    }
    if (b.effort !== undefined) {
      bot.effort = typeof b.effort === 'string' && b.effort.trim() ? b.effort.trim() : null
    }
    if (b.model !== undefined) {
      bot.model = typeof b.model === 'string' && b.model.trim() ? b.model.trim() : null
    }
    if (Array.isArray(b.args)) bot.args_json = JSON.stringify(b.args.map((a) => String(a)))
    if (b.env !== undefined) {
      const env: Record<string, string> = {}
      if (b.env && typeof b.env === 'object') for (const [k, v] of Object.entries(b.env as Rec)) env[k] = String(v)
      bot.env_json = JSON.stringify(env)
    }
    if (b.fast !== undefined) bot.fast = b.fast ? 1 : 0
    if (b.persona !== undefined) bot.persona = typeof b.persona === 'string' && b.persona.trim() ? b.persona.trim() : null
    if (b.autostart !== undefined) bot.autostart = b.autostart ? 1 : 0
    if (b.auto_approve !== undefined) bot.auto_approve = b.auto_approve ? 1 : 0
    if (b.inject_hooks !== undefined) bot.inject_hooks = b.inject_hooks ? 1 : 0
    this.emit('bot_changed', { bot_id: id })
    // API.md §10.2: 只有影響啟動 argv / env 的欄位才需要重啟；只改 autostart → false。
    const LAUNCH_FIELDS = ['model', 'effort', 'fast', 'persona', 'args', 'identity', 'env', 'auto_approve', 'inject_hooks']
    let needs_restart = run !== undefined && LAUNCH_FIELDS.some((k) => b[k] !== undefined)
    // TUI slash 指令當場套用（daemon `apply_live_setting` 同一套條件）：
    // grok `/effort`、grok `/model`（可順便帶 effort）、claude `/model`、claude `/effort`。
    // 清成 CLI 預設沒有對應指令。
    const only = (...fields: string[]) =>
      fields.every((f) => b[f] !== undefined) && LAUNCH_FIELDS.filter((k) => !fields.includes(k)).every((k) => b[k] === undefined)
    if (needs_restart && bot.kind === 'grok' && only('effort') && bot.effort) needs_restart = false
    if (needs_restart && bot.kind === 'grok' && (only('model') || only('model', 'effort')) && bot.model) needs_restart = false
    if (needs_restart && bot.kind === 'claude' && only('model') && bot.model) needs_restart = false
    if (needs_restart && bot.kind === 'claude' && only('effort') && bot.effort) needs_restart = false
    return { needs_restart }
  }

  /** `POST /api/bots/:id/restart`（API.md v3.3）= stop 再 start，回新的 run_id。 */
  private restart(botId: string) {
    const run = this.activeRun(botId)
    if (run) {
      const inFlight = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
      if (inFlight) this.updateTurn(inFlight, { status: 'failed', completed_at: now() })
      run.state = 'stopped'
      run.agent_status = 'unknown'
      run.ended_at = now()
      this.emitBotStatus(botId)
      this.addMessage({
        conversation_id: this.conv(botId),
        turn_id: null,
        bot_id: botId,
        role: 'system',
        content: '套用新設定，重新啟動中（mock）。',
        source: 'system',
        incomplete: 0,
      })
    }
    return this.start(botId)
  }

  /** `DELETE /api/bots/:id`：有 Run 會先 stop（關 pane），設定移除，對話歷史保留。 */
  private deleteBot(id: string) {
    this.bot(id)
    const run = this.activeRun(id)
    if (run) {
      const inFlight = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
      if (inFlight) this.updateTurn(inFlight, { status: 'failed', completed_at: now() })
      run.state = 'stopped'
      run.agent_status = 'unknown'
      run.ended_at = now()
      this.emitBotStatus(id)
    }
    this.bots = this.bots.filter((x) => x.id !== id)
    // messages / turns / conversation 刻意保留（對話歷史不刪）。
    this.emit('bot_changed', { bot_id: id, deleted: true })
    return {}
  }

  private start(botId: string) {
    const bot = this.bot(botId)
    const project = this.projects.find((p) => p.id === bot.project_id)
    if (project && project.host !== 'local') {
      const h = this.hosts.find((x) => x.name === project.host)
      if (!h?.connected) {
        throw new ApiError(409, { error: 'conflict', reason: `主機 ${project.host} 未連線` }, 'conflict')
      }
    }
    const existing = this.activeRun(botId)
    if (existing) throw new ApiError(409, { reason: '已有 active Run', run_id: existing.id }, 'conflict')
    const run: MockRun = {
      id: ulid('run'),
      bot_id: botId,
      state: 'starting',
      agent_status: 'unknown',
      workspace_id: 'ws_demo',
      // herdr 的 pane id 是 `w<workspace>:p<pane>` 這種短字串（不是 ULID）；版面壓力差很多，
      // mock 也照這個形狀走才驗得出標題列擠不擠。
      pane_id: `w${this.runs.length + 1}:p${String.fromCharCode(65 + (this.runs.length % 26))}`,
      own_tab: false,
      adopted: 0,
      native_session_id: null,
      transcript_path: null,
      status: null,
      status_line: null,
      started_at: now(),
      ended_at: null,
    }
    this.runs.push(run)
    this.emitBotStatus(botId)
    setTimeout(() => {
      if (run.state !== 'starting') return
      run.state = 'running'
      run.agent_status = 'idle'
      run.native_session_id = ulid('sess')
      // Only claude ships a statusLine hook; the other kinds render theirs inside the TUI,
      // which is exactly why ChatPanel rebuilds a status bar for them from the store.
      if (this.bot(botId).kind === 'claude') {
        run.status = claudeStatusJson(this.projects.find((p) => p.id === this.bot(botId).project_id)?.path ?? '~')
        run.status_line = 'tony… | OP5 | 26% | 5h 85% | 7d 27% | $18.67'
      }
      this.emitBotStatus(botId)
      this.addMessage({
        conversation_id: this.conv(botId),
        turn_id: null,
        bot_id: botId,
        role: 'system',
        content: `Run ${run.id} 已啟動（mock）。`,
        source: 'system',
        incomplete: 0,
      })
    }, 1400)
    return { run_id: run.id }
  }

  private stop(botId: string) {
    const run = this.activeRun(botId)
    if (!run) return null
    run.state = 'stopping'
    this.emitBotStatus(botId)
    const inFlight = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
    if (inFlight) this.updateTurn(inFlight, { status: 'failed', completed_at: now() })
    setTimeout(() => {
      run.state = 'stopped'
      run.agent_status = 'unknown'
      run.ended_at = now()
      this.emitBotStatus(botId)
      this.addMessage({
        conversation_id: this.conv(botId),
        turn_id: null,
        bot_id: botId,
        role: 'system',
        content: 'Bot 已停止（mock）。',
        source: 'system',
        incomplete: 0,
      })
    }, 700)
    return { ok: true }
  }

  private interrupt(botId: string) {
    const run = this.activeRun(botId)
    if (!run) throw new ApiError(409, { reason: 'Bot 未在執行中' }, 'conflict')
    const inFlight = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
    if (inFlight) this.updateTurn(inFlight, { status: 'failed', completed_at: now() })
    run.agent_status = 'idle'
    this.emitBotStatus(botId)
    this.addMessage({
      conversation_id: this.conv(botId),
      turn_id: null,
      bot_id: botId,
      role: 'system',
      content: '已送出 esc（interrupt）。',
      source: 'system',
      incomplete: 0,
    })
    return { ok: true }
  }

  /**
   * `POST /api/bots/:id/abort` — 強制結束目前回合（daemon 的 `abort_turns`）。
   * 和 `interrupt` 的差別就在這裡：沒有 active run 也不是錯誤，掛著的回合照收。
   */
  private abort(botId: string) {
    const run = this.activeRun(botId)
    const stuck = this.turns.filter((t) => {
      const conv = this.conversations.get(botId)
      if (t.conversation_id !== conv) return false
      return t.status === 'in_flight' || t.delivery === 'unknown'
    })
    for (const t of stuck) {
      this.updateTurn(t, { status: 'failed', delivery: t.delivery === 'unknown' ? 'failed' : t.delivery, completed_at: now() })
      this.addMessage({
        conversation_id: t.conversation_id,
        turn_id: t.id,
        bot_id: botId,
        role: 'system',
        content: '回合已由使用者強制中止',
        source: 'system',
        incomplete: 0,
      })
    }
    if (run) {
      run.agent_status = 'idle'
      this.emitBotStatus(botId)
    }
    return { aborted: stuck.map((t) => t.id), keys_sent: Boolean(run), key_error: run ? null : 'no active run' }
  }

  /**
   * `POST /api/bots/:id/login` — 對這個 bot 的 TUI 送 `/login`。
   *
   * 擋下來的理由跟 daemon 同一組 key，好讓前端的文案對照表在 mock 下也走得到；codex 沒有
   * TUI 內的登入指令，所以是 400 而不是 409。
   */
  private login(botId: string) {
    const bot = this.bots.find((x) => x.id === botId)
    if (!bot) throw new ApiError(404, { error: 'not_found', what: 'bot' }, 'not found')
    if (bot.kind === 'codex') {
      throw new ApiError(
        400,
        { error: 'login_unsupported', kind: bot.kind, message: 'codex has no in-session login command' },
        'bad request',
      )
    }
    const run = this.activeRun(botId)
    if (!run) throw new ApiError(409, { error: 'conflict', reason: 'not_running', bot_id: botId }, 'conflict')
    if (run.state !== 'running') {
      throw new ApiError(409, { error: 'conflict', reason: 'not_running', bot_id: botId, run_id: run.id }, 'conflict')
    }
    if (run.agent_status === 'working' || run.agent_status === 'blocked') {
      throw new ApiError(409, { error: 'conflict', reason: 'agent_busy', bot_id: botId, run_id: run.id }, 'conflict')
    }
    if (this.turns.some((t) => t.run_id === run.id && t.status === 'in_flight')) {
      throw new ApiError(409, { error: 'conflict', reason: 'turn_in_flight', bot_id: botId, run_id: run.id }, 'conflict')
    }
    this.addMessage({
      conversation_id: this.conv(botId),
      turn_id: null,
      bot_id: botId,
      role: 'system',
      content: '已送出 /login，agent 會停在登入畫面，完成登入前不會工作。',
      source: 'system',
      incomplete: 0,
    })
    return { run_id: run.id, kind: bot.kind, command: '/login' }
  }

  private prompt(botId: string, b: Rec, groupId: string | null = null, replyOverride: string | null = null) {
    const run = this.activeRun(botId)
    if (!run) throw new ApiError(409, { error: 'conflict', reason: 'bot has no active run' }, 'conflict')
    if (run.state !== 'running') {
      throw new ApiError(409, { error: 'conflict', reason: 'run is not running', state: run.state }, 'conflict')
    }
    if (run.agent_status === 'blocked') {
      throw new ApiError(409, { error: 'conflict', reason: 'agent is blocked; answer the prompt first' }, 'conflict')
    }
    const clientRequestId = typeof b.client_request_id === 'string' ? b.client_request_id : null
    if (clientRequestId) {
      const dup = this.turns.find((t) => t.client_request_id === clientRequestId)
      if (dup) return { turn_id: dup.id, message_id: null, delivery: dup.delivery }
    }
    const busy = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
    if (busy) throw new ApiError(409, { error: 'conflict', reason: 'a turn is already in flight', turn_id: busy.id }, 'conflict')
    const unknown = this.turns.find((t) => t.run_id === run.id && t.delivery === 'unknown')
    if (unknown) {
      throw new ApiError(
        409,
        { error: 'conflict', reason: 'a previous turn has unknown delivery; abandon it first', turn_id: unknown.id },
        'conflict',
      )
    }

    const text = String(b.text ?? '')
    const attachIds = Array.isArray(b.attachments) ? b.attachments.filter((x): x is string => typeof x === 'string') : []
    const turn: MockTurn = {
      id: ulid('turn'),
      conversation_id: this.conv(botId),
      run_id: run.id,
      bot_id: botId,
      origin: 'web',
      status: 'in_flight',
      delivery: 'pending',
      client_request_id: clientRequestId,
      created_at: now(),
      completed_at: null,
    }
    this.turns.push(turn)
    const userMsg = this.addMessage({
      conversation_id: turn.conversation_id,
      turn_id: turn.id,
      bot_id: botId,
      role: 'user',
      content: text,
      source: 'web',
      incomplete: 0,
      group_id: groupId,
      attachments_json: attachIds.length
        ? JSON.stringify(
            attachIds.map((id) => ({
              id,
              name: `image-${id.slice(-4)}.png`,
              mime: this.blobs.get(id)?.type ?? 'image/png',
              size: this.blobs.get(id)?.size ?? 0,
              path: `/Users/me/project/agents-manager/.agents-manager/attachments/${id}.png`,
            })),
          )
        : null,
    })
    this.updateTurn(turn, { delivery: 'ok' })
    run.agent_status = 'working'
    this.emitBotStatus(botId)

    const lowered = text.toLowerCase()
    if (lowered.includes('blocked') || lowered.includes('rm -rf')) {
      setTimeout(() => this.enterBlocked(botId), 900)
    } else if (lowered.includes('retry')) {
      // v4.2: the CLI is retrying an upstream failure. The spinner keeps spinning and the turn
      // stays in flight, so the only signal is `turn_progress.alert`.
      const banners = [
        'API error · Retrying in 0s · attempt 1/10',
        'API error · Retrying in 4s · attempt 2/10',
      ]
      banners.forEach((alert, i) => {
        setTimeout(() => {
          if (turn.status !== 'in_flight') return
          this.emit('turn_progress', {
            bot_id: botId,
            run_id: run.id,
            turn_id: turn.id,
            text: '',
            activity: 'Retrying… (18s · ↑ 0.9k tokens)',
            alert,
            revision: i + 1,
          })
        }, 300 * (i + 1))
      })
      setTimeout(() => this.finishTurn(botId, turn, 'hook', '重試後成功了。'), 9000)
    } else if (lowered.includes('fallback')) {
      setTimeout(() => this.finishTurn(botId, turn, 'terminal_fallback'), 2600)
    } else {
      // v3.9 live output: 3–4 `turn_progress` frames (every 0.5 s) before the final reply.
      const reply = replyOverride ?? this.nextReply()
      const slow = lowered.includes('slow')
      const frames = slow ? 4 : 3
      // v4.1: the thinking phase first — frames with only `activity` and an empty `text`,
      // the state the real daemon sits in while the pane shows nothing but the spinner.
      // The real verb is randomised per frame (Thinking / Boogieing / Puttering / …), so these
      // are just two of them; only the bracketed counter shape is meaningful.
      const thinking = ['Boogieing… (2s · ↑ 0.4k tokens)', 'Puttering… (4s · ↑ 1.2k tokens)']
      thinking.forEach((activity, i) => {
        setTimeout(() => {
          if (turn.status !== 'in_flight') return
          this.emit('turn_progress', { bot_id: botId, run_id: run.id, turn_id: turn.id, text: '', activity, revision: i + 1 })
        }, 200 * (i + 1))
      })
      const lines = reply.split('\n')
      // Grow by whole lines when there are several (a fence never splits mid-way), else by chars.
      const partial = (i: number) =>
        lines.length > 1
          ? lines.slice(0, Math.max(1, Math.ceil((lines.length * i) / (frames + 1)))).join('\n')
          : reply.slice(0, Math.max(1, Math.round((reply.length * i) / (frames + 1))))
      for (let i = 1; i <= frames; i++) {
        setTimeout(() => {
          if (turn.status !== 'in_flight') return
          this.emit('turn_progress', {
            bot_id: botId,
            run_id: run.id,
            turn_id: turn.id,
            text: partial(i),
            activity: 'Simmering… (6s · ↑ 2.1k tokens)',
            revision: thinking.length + i,
          })
        }, 500 * i)
      }
      setTimeout(() => this.finishTurn(botId, turn, 'hook', reply), slow ? 8000 : 500 * (frames + 1) + 400)
    }
    return { turn_id: turn.id, message_id: userMsg.id, delivery: 'ok' }
  }

  private nextReply(): string {
    return REPLIES[this.replyIndex++ % REPLIES.length]
  }

  private finishTurn(botId: string, turn: MockTurn, source: 'hook' | 'terminal_fallback', reply = this.nextReply()) {
    if (turn.status !== 'in_flight') return
    const run = this.activeRun(botId)
    this.updateTurn(turn, {
      status: source === 'hook' ? 'completed' : 'completed_fallback',
      completed_at: now(),
    })
    this.addMessage({
      conversation_id: turn.conversation_id,
      turn_id: turn.id,
      bot_id: botId,
      role: 'assistant',
      content: source === 'hook' ? reply : reply.split('\n')[0],
      source,
      incomplete: source === 'terminal_fallback' ? 1 : 0,
    })
    if (run) {
      run.agent_status = 'idle'
      this.emitBotStatus(botId)
    }
  }

  /** Public so the dev helper can force a blocked state without a prompt. */
  enterBlocked(botId: string) {
    const run = this.activeRun(botId)
    if (!run) return
    run.agent_status = 'blocked'
    this.emitBotStatus(botId)
  }

  private keys(botId: string, b: Rec) {
    const run = this.activeRun(botId)
    if (!run) throw new ApiError(409, { reason: 'Bot 未在執行中' }, 'conflict')
    const expect = b.expect_run_id
    if (typeof expect === 'string' && expect !== run.id) {
      throw new ApiError(409, { reason: 'expect_run_id 與目前 Run 不符', run_id: run.id }, 'conflict')
    }
    const keys = (Array.isArray(b.keys) ? b.keys : []).map(String)
    if (run.agent_status === 'blocked') {
      const affirm = keys.some((k) => ['y', 'enter', 'Enter'].includes(k))
      if (affirm) {
        run.agent_status = 'working'
        this.emitBotStatus(botId)
        const turn = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
        if (turn) setTimeout(() => this.finishTurn(botId, turn, 'hook'), 1500)
        else setTimeout(() => this.setIdle(botId), 1200)
      } else if (keys.some((k) => ['n', 'esc', 'Esc', 'ctrl+c'].includes(k))) {
        const turn = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
        if (turn) this.updateTurn(turn, { status: 'failed', completed_at: now() })
        this.setIdle(botId)
      }
    }
    return { ok: true, keys }
  }

  /** `POST /bots/:id/text` — 整段文字打進 pane。多行原樣留著，Enter 是分開的一顆鍵。 */
  private text(botId: string, b: Rec) {
    const run = this.activeRun(botId)
    if (!run) throw new ApiError(409, { reason: 'Bot 未在執行中' }, 'conflict')
    const expect = b.expect_run_id
    if (typeof expect === 'string' && expect !== run.id) {
      throw new ApiError(409, { reason: 'expect_run_id 與目前 Run 不符', run_id: run.id }, 'conflict')
    }
    return { ok: true, text: String(b.text ?? ''), enter: b.enter !== false }
  }

  private setIdle(botId: string) {
    const run = this.activeRun(botId)
    if (!run) return
    run.agent_status = 'idle'
    this.emitBotStatus(botId)
  }

  private abandon(turnId: string) {
    const turn = this.turns.find((t) => t.id === turnId)
    if (!turn) throw new ApiError(404, { reason: 'turn not found' }, 'not found')
    this.updateTurn(turn, { status: 'failed', completed_at: now() })
    this.setIdle(turn.bot_id)
    return { ok: true }
  }

  private messagesOf(botId: string) {
    return {
      bot_id: botId,
      conversation_id: this.conv(botId),
      messages: this.messages.filter((m) => m.bot_id === botId),
      turns: this.turns.filter((t) => t.bot_id === botId),
      has_more: false,
    }
  }

  // ------------------------------------------------------------ group chat (§13)

  /** Mirrors `daemon/src/group.rs::messages`: member bots' messages merged, paginated by id. */
  private projectMessages(projectId: string, q: URLSearchParams) {
    if (!this.projects.some((p) => p.id === projectId)) {
      throw new ApiError(404, { error: 'not_found', what: 'project' }, 'not found')
    }
    const limit = Math.min(500, Math.max(1, Number(q.get('limit') ?? 100) || 100))
    const before = q.get('before') ?? ''
    const byId = new Map(this.bots.map((b) => [b.id, b] as const))
    const rows = this.messages
      .filter((m) => byId.get(m.bot_id)?.project_id === projectId)
      .filter((m) => !before || m.id < before)
      .sort((a, b) => (a.id < b.id ? 1 : a.id > b.id ? -1 : 0))
    const page = rows.slice(0, limit).reverse()
    return {
      project_id: projectId,
      messages: page.map((m) => ({ ...m, bot_name: byId.get(m.bot_id)?.name ?? m.bot_id })),
      has_more: rows.length > limit,
    }
  }

  /** Mirrors `daemon/src/group.rs::chat` — mention parsing, fan-out, skipped notes. */
  private projectChat(projectId: string, b: Rec) {
    if (!this.projects.some((p) => p.id === projectId)) {
      throw new ApiError(404, { error: 'not_found', what: 'project' }, 'not found')
    }
    const text = String(b.text ?? '')
    const crid = typeof b.client_request_id === 'string' && b.client_request_id ? b.client_request_id : ulid('crid')
    const members = this.bots.filter((x) => x.project_id === projectId)
    const targets = parseMentions(text, members)
    if (targets.length === 0) {
      throw new ApiError(
        400,
        {
          error: 'no_mention',
          message: 'text must mention @all or at least one bot of this project',
          bots: members.map((m) => ({ id: m.id, name: m.name, kind: m.kind })),
        },
        'no mention',
      )
    }
    const sent: Rec[] = []
    const skipped: Rec[] = []
    for (const t of targets) {
      try {
        const out = this.prompt(t.id, { text, client_request_id: `${crid}:${t.id}` }, crid) as Rec
        sent.push({ bot_id: t.id, bot_name: t.name, turn_id: out.turn_id, message_id: out.message_id, delivery: out.delivery })
      } catch (e) {
        const reason = e instanceof ApiError ? String(e.body.reason ?? e.message) : String(e)
        const code = /no active run|not running/.test(reason)
          ? 'not_running'
          : /blocked/.test(reason)
            ? 'blocked'
            : /in flight/.test(reason)
              ? 'in_flight'
              : /unknown delivery/.test(reason)
                ? 'unknown_delivery'
                : 'conflict'
        const label: Record<string, string> = {
          not_running: 'bot 未啟動（不會自動啟動）',
          blocked: 'agent 正在等待終端回應',
          in_flight: '上一回合仍在進行中',
          unknown_delivery: '上一回合送達狀態未知，請先放棄該回合',
        }
        const dup = this.messages.some((m) => m.bot_id === t.id && m.group_id === crid && m.role === 'system')
        if (!dup) {
          this.addMessage({
            conversation_id: this.conv(t.id),
            turn_id: null,
            bot_id: t.id,
            role: 'system',
            content: `群組訊息未送達 ${t.name}：${label[code] ?? reason}`,
            source: 'system',
            incomplete: 0,
            group_id: crid,
          })
        }
        skipped.push({ bot_id: t.id, bot_name: t.name, reason: code, detail: reason })
      }
    }
    return { group_id: crid, project_id: projectId, sent, skipped }
  }

  // ---------------------------------------------------------------- 主機 shell

  /**
   * `POST /api/hosts/:name/shells` 起的假 shell。有一個真的行緩衝，`shellText` 會依指令
   * 追加輸出——`VITE_MOCK=1` 下要能看出「打了、送了、終端有反應」，不然這個面板的
   * 輸入框、歷史與按鍵列在 mock 裡全都試不出來。
   */
  private openShell(host: string, cwd: string) {
    if (host !== 'local') {
      const h = this.host(host)
      if (!h.connected) throw new ApiError(502, { error: 'upstream', message: `host \`${host}\` is not connected` }, 'upstream')
    } else if (!this.connected) {
      throw new ApiError(502, { error: 'upstream', message: 'host `local` is not connected' }, 'upstream')
    }
    if (this.shellsOf(host).length >= 8) {
      throw new ApiError(409, { error: 'conflict', reason: 'too_many_shells', host, max: 8 }, 'conflict')
    }
    const dir = cwd.trim() || this.projects.find((p) => p.host === host)?.path || (host === 'local' ? '/Users/m1pro' : `/Users/${host}`)
    const shell: MockShell = {
      host,
      pane_id: `w9:s${++this.shellSeq}`,
      tab_id: `w9:t${this.shellSeq}`,
      workspace_id: 'w9',
      cwd: dir,
      created_at: now(),
      lines: [`Last login: ${now().slice(11, 19)} on ttys00${this.shellSeq}`, ''],
      typed: '',
    }
    this.shells.set(`${host}/${shell.pane_id}`, shell)
    return this.shellJson(shell)
  }

  private shellJson(s: MockShell) {
    const { host, pane_id, tab_id, workspace_id, cwd, created_at } = s
    return { host, pane_id, tab_id, workspace_id, cwd, created_at }
  }

  private shellsOf(host: string) {
    return [...this.shells.values()].filter((s) => s.host === host).map((s) => this.shellJson(s))
  }

  /** 白名單就是這張表：不是 mock 自己開的 pane 一律 404，跟 daemon 同一條規則。 */
  private shell(host: string, paneId: string): MockShell {
    const s = this.shells.get(`${host}/${paneId}`)
    if (!s) throw new ApiError(404, { error: 'not_found', what: 'shell' }, 'shell not found')
    return s
  }

  private closeShell(host: string, paneId: string) {
    this.shells.delete(`${host}/${paneId}`)
    return {}
  }

  private shellTerminal(host: string, paneId: string, source: string, lines: number) {
    const s = this.shell(host, paneId)
    const all = [...s.lines, `${s.cwd.split('/').pop() ?? '~'} % ${s.typed}`]
    const shown = all.slice(Math.max(0, all.length - lines))
    return {
      host,
      pane_id: paneId,
      cwd: s.cwd,
      source,
      text: shown.join('\n'),
      revision: this.seq,
      truncated: all.length > lines,
      columns: 120,
      rows: 30,
    }
  }

  private shellText(host: string, paneId: string, text: string, enter: boolean) {
    const s = this.shell(host, paneId)
    s.typed += text
    if (!enter) return {}
    const cmd = s.typed.trim()
    s.typed = ''
    s.lines.push(`${s.cwd.split('/').pop() ?? '~'} % ${cmd}`)
    if (cmd === 'clear') {
      s.lines = []
      return {}
    }
    const echo = /^echo\s+(.*)$/.exec(cmd)
    if (cmd === '') {
      /* 只按 Enter：提示符再來一行就好 */
    } else if (echo) {
      s.lines.push(echo[1].replace(/^["']|["']$/g, ''))
    } else if (cmd === 'pwd') {
      s.lines.push(s.cwd)
    } else if (cmd === 'hostname') {
      s.lines.push(host === 'local' ? 'm1pro.local' : `${host}.local`)
    } else if (cmd.startsWith('ls')) {
      s.lines.push('Cargo.toml  daemon      docs        web')
    } else if (cmd === 'gh auth status') {
      s.lines.push('github.com', '  ✓ Logged in to github.com account Eden-Sun (keyring)', '  - Active account: true')
    } else {
      s.lines.push(`zsh: command not found: ${cmd.split(/\s+/)[0]}`)
    }
    s.lines.push('')
    return {}
  }

  private shellKeys(host: string, paneId: string, keys: unknown) {
    const s = this.shell(host, paneId)
    const list = Array.isArray(keys) ? keys.map((k) => String(k)) : []
    if (list.length === 0) throw new ApiError(400, { error: 'bad_request', message: 'keys must not be empty' }, 'bad request')
    for (const k of list) {
      if (k === 'ctrl+c') {
        s.lines.push(`${s.cwd.split('/').pop() ?? '~'} % ${s.typed}^C`, '')
        s.typed = ''
      } else if (k === 'esc' || k === 'tab') {
        /* 在假 shell 裡沒有可觀察的效果，但不能是錯誤：真的 shell 也收得下 */
      } else if (k === 'enter') {
        this.shellText(host, paneId, '', true)
      } else if (k === 'up' || k === 'down') {
        /* 真 shell 會走它自己的歷史；mock 不模擬 */
      }
    }
    return {}
  }

  private terminal(botId: string, source: string, lines: number) {
    const bot = this.bot(botId)
    const run = this.activeRun(botId)
    const status = run?.agent_status ?? 'unknown'
    const head = [
      `$ ${bot.kind} ${JSON.parse(bot.args_json).join(' ')}`.trimEnd(),
      `  pane=${run?.pane_id ?? '-'}  run=${run?.id ?? '-'}  status=${status}`,
      '',
    ]
    const body =
      status === 'blocked'
        ? [
            '⏺ Bash(rm -rf ./target/debug)',
            '  ⎿  這個指令會刪除建置產物。',
            '',
            '╭──────────────────────────────────────────────╮',
            '│  是否允許執行？                               │',
            '│                                              │',
            '│  ❯ 1. Yes                                    │',
            '│    2. Yes, and don’t ask again              │',
            '│    3. No, and tell Claude what to do (esc)   │',
            '╰──────────────────────────────────────────────╯',
            '',
            '  按 y / Enter 允許，n 或 Esc 取消。',
          ]
        : status === 'working'
          ? ['⏺ 正在思考…', '  ⎿  Read(src/api/transport.ts)', '  ⎿  Grep("X-AM-Token")', '', '  ✻ Thinking… (7s)']
          : ['⏺ 已完成上一個回合。', '', '> ', '  ? for shortcuts']
    const all = [...head, ...body]
    const text = all.slice(Math.max(0, all.length - lines)).join('\n')
    return {
      text,
      revision: this.seq,
      truncated: all.length > lines,
      source,
      pane_id: run?.pane_id ?? null,
      // pane 幾何：窄 pane 警示與「移到自己的分頁」按鈕都靠這個欄位判斷。
      columns: run ? this.paneColumns(run) : null,
      rows: run ? 27 : null,
    }
  }

  // ------------------------------------------------------- Issue Team (SPEC-team)

  private team(id: string): MockTeam {
    const t = this.teams.find((x) => x.id === id)
    if (!t) throw new ApiError(404, { error: 'not_found', what: 'team' }, 'team not found')
    return t
  }

  private teamJson(t: MockTeam): Rec {
    const tasks = this.teamTasks.filter((x) => x.team_id === t.id)
    const summary: Rec = { total: tasks.length }
    for (const task of tasks) summary[task.state] = Number(summary[task.state] ?? 0) + 1
    const issuesSummary = {
      total: t.issues.length,
      done: t.issues.filter((issue) => issue.state === 'done').length,
      failed: t.issues.filter((issue) => issue.state === 'failed' || issue.state === 'skipped').length,
      queued: t.issues.filter((issue) => issue.state === 'queued').length,
    }
    return {
      id: t.id,
      issues: t.issues.map((issue) => ({ ...issue })),
      current_issue_id: t.issues.find((issue) => issue.state === 'working')?.id ?? null,
      issues_summary: issuesSummary,
      project_id: t.project_id,
      issue_number: t.issue_number,
      issue_title: t.issue_title,
      issue_url: t.issue_url,
      phase: t.phase,
      pause_reason: t.pause_reason,
      branch: t.branch,
      deliver: t.deliver,
      supervised: t.supervised,
      members: t.members.map((m) => ({ ...m, deleted: !this.bots.some((bot) => bot.id === m.bot_id) })),
      tasks_summary: summary,
      budget: { ...t.budget },
      usage: { ...t.usage, per_bot: { ...t.usage.per_bot } },
      pr_url: t.pr_url,
      issue_closed_at: t.issue_closed_at,
      created_at: t.created_at,
      started_at: t.started_at,
      ended_at: t.ended_at,
    }
  }

  private taskJson(task: MockTeamTask): Rec {
    return {
      id: task.id,
      seq: task.seq,
      title: task.title,
      brief: task.brief,
      files: [...task.files],
      worker_bot_id: task.worker_bot_id,
      branch: task.branch,
      state: task.state,
      round: task.round,
      last_report: task.last_report,
      last_verdict: task.last_verdict,
      merge_sha: task.merge_sha,
      updated_at: task.updated_at,
    }
  }

  private teamDetail(id: string) {
    const t = this.team(id)
    return {
      ...this.teamJson(t),
      tasks: this.teamTasks.filter((x) => x.team_id === id).map((x) => this.taskJson(x)),
      summary: t.summary,
      base_ref: t.base_ref,
      base_sha: t.base_sha,
      worktree_root: t.worktree_root,
    }
  }

  private teamEventsOf(id: string) {
    this.team(id)
    return this.teamEvents
      .filter((e) => e.team_id === id)
      .map((e) => ({ ...e, payload: { ...e.payload } }))
  }

  private emitTeam(t: MockTeam) {
    this.emit('team_changed', {
      team_id: t.id,
      project_id: t.project_id,
      phase: t.phase,
      pause_reason: t.pause_reason,
      usage: { ...t.usage },
    })
  }

  private emitTask(task: MockTeamTask) {
    task.updated_at = now()
    this.emit('team_task_updated', { team_id: task.team_id, task: this.taskJson(task) })
  }

  private teamEvent(
    teamId: string,
    kind: MockTeamEvent['kind'],
    payload: Rec,
    extra: Partial<MockTeamEvent> = {},
  ): MockTeamEvent {
    const ev: MockTeamEvent = {
      id: ulid('tev'),
      team_id: teamId,
      kind,
      from_bot_id: null,
      to_bot_id: null,
      task_id: null,
      turn_id: null,
      status: kind === 'relay' ? 'delivered' : null,
      payload,
      created_at: now(),
      ...extra,
    }
    this.teamEvents.push(ev)
    this.emit('team_event', { team_id: teamId, event: { ...ev } })
    return ev
  }

  private setPhase(t: MockTeam, phase: TeamPhase, reason: string | null = null) {
    const from = t.phase
    t.phase = phase
    t.pause_reason = phase === 'paused' ? reason : null
    if (phase === 'done' || phase === 'aborted' || phase === 'failed') t.ended_at = now()
    this.teamEvent(t.id, 'phase', { from, to: phase, reason })
    this.emitTeam(t)
  }

  /** daemon 代發的一則 relay：收件 bot 的一則普通 user 訊息 + 一筆 `team_events`。 */
  private teamRelay(t: MockTeam, fromBotId: string | null, toBotId: string, text: string, taskId: string | null = null) {
    t.usage.relays += 1
    this.addMessage({
      conversation_id: this.conv(toBotId),
      turn_id: null,
      bot_id: toBotId,
      role: 'user',
      content: text,
      source: 'web',
      incomplete: 0,
      team_id: t.id,
      relay_from: fromBotId,
    })
    this.teamEvent(t.id, 'relay', { action: 'relay', text_excerpt: text.slice(0, 120) }, {
      from_bot_id: fromBotId,
      to_bot_id: toBotId,
      task_id: taskId,
    })
    const run = this.activeRun(toBotId)
    if (run && run.state === 'running') {
      run.agent_status = 'working'
      this.emitBotStatus(toBotId)
    }
    this.emitTeam(t)
  }

  /** 成員的回覆（assistant 訊息，含 am-team 區塊）。 */
  private teamReply(t: MockTeam, botId: string, text: string) {
    t.usage.per_bot[botId] = { turns: (t.usage.per_bot[botId]?.turns ?? 0) + 1 }
    this.addMessage({
      conversation_id: this.conv(botId),
      turn_id: null,
      bot_id: botId,
      role: 'assistant',
      content: text,
      source: 'hook',
      incomplete: 0,
      team_id: t.id,
    })
    const run = this.activeRun(botId)
    if (run && run.state === 'running') {
      run.agent_status = 'idle'
      this.emitBotStatus(botId)
    }
  }

  private memberBot(t: MockTeam, role: TeamRole, index = 0): MockBot | undefined {
    const ids = t.members.filter((m) => m.role === role).map((m) => m.bot_id)
    const id = ids[index]
    return id ? this.bots.find((b) => b.id === id) : undefined
  }

  private roleSpec(v: unknown): { kind: BotKind; model: string | null; effort: string | null; fast: boolean; identity: string | null; persona_extra: string } {
    const o = (v ?? {}) as Rec
    return {
      kind: toKind(o.kind),
      model: typeof o.model === 'string' && o.model.trim() ? o.model.trim() : null,
      effort: typeof o.effort === 'string' && o.effort.trim() ? o.effort.trim() : null,
      fast: o.fast === true,
      identity: typeof o.identity === 'string' && o.identity.trim() ? o.identity.trim() : null,
      persona_extra: typeof o.persona_extra === 'string' ? o.persona_extra : '',
    }
  }

  /** `POST /api/projects/:id/teams` — 建 team、建成員、背景啟動，之後由假 scheduler 推進。 */
  private createTeam(projectId: string, b: Rec) {
    const p = this.projects.find((x) => x.id === projectId)
    if (!p) throw new ApiError(404, { error: 'not_found', what: 'project' }, 'not found')
    // SPEC-team §10.1：非 git 目錄 400 `not_a_git_repo`；mock 用 github 有無代替。
    if (!p.github) throw new ApiError(400, { error: 'not_a_git_repo' }, 'not a git repo')
    // SPEC-team §2.3 之後前端送的是 issue **佇列**（`issue_numbers`）；舊的單數欄位仍接受。
    const queue = Array.isArray(b.issue_numbers) ? b.issue_numbers.map((n) => Number(n) || 0).filter(Boolean) : []
    const issueNumber = queue[0] ?? (Number(b.issue_number ?? 0) || 0)
    const issue = ISSUES.find((i) => i.number === issueNumber)
    if (!issue) throw new ApiError(502, { error: 'upstream', message: `gh: issue #${issueNumber} not found` }, 'upstream')

    const pm = this.roleSpec(b.pm)
    const workersRaw = (b.workers ?? {}) as Rec
    const workers = { ...this.roleSpec(workersRaw), count: Math.max(1, Math.min(TEAM_WORKERS_MAX, Number(workersRaw.count ?? 2) || 2)) }
    const reviewer = b.reviewer === null || b.reviewer === undefined ? null : this.roleSpec(b.reviewer)
    const budget: TeamBudget = { ...TEAM_BUDGET_DEFAULTS, ...((b.budget ?? {}) as Partial<TeamBudget>) }

    // §9.2 預檢：任一角色的 kind 已用量 ≥ quota_stop_pct → 400 quota_low。
    for (const kind of new Set([pm.kind, workers.kind, ...(reviewer ? [reviewer.kind] : [])])) {
      const q = this.quota[kind]
      if (!q) continue
      for (const w of [q.five_hour, q.seven_day]) {
        const used = Number((w as Rec | null)?.used_pct ?? 0)
        if (used >= budget.quota_stop_pct) {
          throw new ApiError(400, { error: 'quota_low', kind, used_pct: used }, 'quota low')
        }
      }
    }

    const id = ulid('team')
    const tid6 = id.slice(-6).toLowerCase()
    const branch = `team/i${issueNumber}-${tid6}`
    const root = `/Users/me/.config/agents-manager/teams/${id}`
    const team: MockTeam = {
      id,
      project_id: projectId,
      issue_number: issueNumber,
      issue_title: String(issue.title),
      issue_url: `${p.github.url}/issues/${issueNumber}`,
      phase: 'starting',
      pause_reason: null,
      resume_phase: null,
      base_ref: String(b.base ?? 'HEAD') || 'HEAD',
      base_sha: 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678',
      branch,
      worktree_root: root,
      deliver: b.deliver === 'pr' ? 'pr' : 'branch',
      supervised: b.supervised === true,
      budget,
      usage: { relays: 0, review_rounds_total: 0, elapsed_min: 0, per_bot: {} },
      members: [],
      issues: [{
        id: ulid('issue'),
        seq: 1,
        issue_number: issueNumber,
        issue_title: String(issue.title),
        issue_url: `${p.github.url}/issues/${issueNumber}`,
        state: 'working',
        branch,
        summary: null,
        pr_url: null,
        issue_closed_at: null,
        fail_reason: null,
        started_at: now(),
        ended_at: null,
      }],
      pr_url: null,
      summary: null,
      issue_closed_at: null,
      created_at: now(),
      started_at: now(),
      ended_at: null,
    }

    const mk = (role: TeamRole, name: string, spec: ReturnType<typeof this.roleSpec>, cwd: string) => {
      const bot: MockBot = {
        id: ulid('bot'),
        project_id: projectId,
        name,
        kind: spec.kind,
        model: spec.model,
        effort: spec.effort,
        fast: spec.fast ? 1 : 0,
        persona: `你是 issue #${issueNumber} 的 ${role}。${spec.persona_extra}`.trim(),
        args_json: '[]',
        autostart: 0,
        inject_hooks: 1,
        auto_approve: 1,
        identity: spec.identity,
        env_json: '{}',
        managed_by: 'team',
        team_id: id,
        team_role: role,
        cwd,
        created_at: now(),
      }
      this.bots.push(bot)
      team.members.push({ bot_id: bot.id, role })
      return bot
    }

    mk('pm', `i${issueNumber}-pm`, pm, `${root}/main`)
    for (let i = 1; i <= workers.count; i++) mk('worker', `i${issueNumber}-dev-${i}`, workers, `${root}/dev-${i}`)
    if (reviewer) mk('reviewer', `i${issueNumber}-rev`, reviewer, `${root}/reviewer`)

    this.teams.push(team)
    this.teamEvent(id, 'note', { text: `已建立 worktree ${root}，整合分支 ${branch}` })
    this.emit('bot_changed', { team_id: id })
    this.emitTeam(team)
    // 成員背景啟動（沿用既有 start()，1.4 秒後 running）。
    for (const m of team.members) this.start(m.bot_id)
    this.buildTeamSteps(team)
    this.scheduleTeam(team, 2600)
    return { team_id: id }
  }

  /** 假 scheduler：把一整輪流程拆成一連串步驟，每 1.6 秒推進一步。 */
  private buildTeamSteps(t: MockTeam) {
    const steps: (() => void)[] = []
    const pmBot = this.memberBot(t, 'pm')
    const workers = t.members.filter((m) => m.role === 'worker').map((m) => this.bots.find((b) => b.id === m.bot_id)!)
    const rev = this.memberBot(t, 'reviewer')
    const short = (b: MockBot) => b.name.replace(/^i\d+-/, '')
    // 有 reviewer 時，挑一件 task 走一次 request_changes，讓 round 機制看得到。
    const reworkAt = workers.length > 1 ? 1 : 0
    // Task 物件先建好（步驟閉包要用），但要到 dispatch 那一步才進 `this.teamTasks`。
    const tasks: MockTeamTask[] = workers.map((w, i) => {
      const seed = TEAM_TASK_SEEDS[i % TEAM_TASK_SEEDS.length]
      return {
        id: ulid('task'),
        team_id: t.id,
        seq: i + 1,
        title: seed.title,
        brief: seed.brief,
        files: [...seed.files],
        worker_bot_id: w.id,
        branch: `${t.branch}/t${i + 1}-${short(w)}`,
        state: 'queued',
        round: 0,
        last_report: null,
        last_verdict: null,
        merge_sha: null,
        created_at: now(),
        updated_at: now(),
      }
    })

    steps.push(() => {
      this.setPhase(t, 'planning')
      if (pmBot) {
        this.teamRelay(
          t,
          DAEMON_SENDER,
          pmBot.id,
          `Issue #${t.issue_number}「${t.issue_title}」。全文在 \`.agents-manager/team/ISSUE.md\`。\n目前有 ${workers.length} 位執行者可派：${workers.map(short).join('、')}。請先讀取檔案再派工。`,
        )
      }
    })

    steps.push(() => {
      for (const task of tasks) {
        this.teamTasks.push(task)
        this.emitTask(task)
      }
      if (pmBot) {
        this.teamReply(
          t,
          pmBot.id,
          `我把 issue #${t.issue_number} 拆成 ${tasks.length} 件互不重疊的 task（以檔案切分）：\n\n` +
            tasks.map((x) => `- **t${x.seq}**（${short(this.bots.find((b) => b.id === x.worker_bot_id)!)}）${x.title}`).join('\n') +
            '\n\n' +
            amTeam({
              action: 'dispatch',
              tasks: tasks.map((x) => ({
                to: short(this.bots.find((b) => b.id === x.worker_bot_id)!),
                title: x.title,
                brief: x.brief,
                files: x.files,
              })),
            }),
        )
      }
    })

    steps.push(() => {
      this.setPhase(t, 'working')
      for (const task of tasks) {
        task.state = 'working'
        this.emitTask(task)
        this.teamRelay(
          t,
          pmBot?.id ?? null,
          task.worker_bot_id,
          `Task t${task.seq}「${task.title}」（分支 \`${task.branch}\` 已建好並 checkout）。\n\n${task.brief}\n\n相關檔案：${task.files.join('、')}`,
          task.id,
        )
      }
    })

    const report = (task: MockTeamTask, again: boolean) => () => {
      const seed = TEAM_TASK_SEEDS[(task.seq - 1) % TEAM_TASK_SEEDS.length]
      const body = again ? `已依 reviewer 的意見修正並補測試。\n\n${seed.report}` : seed.report
      task.last_report = body.split('\n')[0]
      this.teamReply(t, task.worker_bot_id, `${body}\n\n${amTeam({ action: 'report', status: 'done', summary: task.last_report })}`)
      task.state = rev ? 'reviewing' : 'merging'
      this.emitTask(task)
      if (rev) {
        this.teamRelay(
          t,
          task.worker_bot_id,
          rev.id,
          `請審查 task t${task.seq}「${task.title}」（分支 \`${task.branch}\`，第 ${task.round + 1} 回）。\n執行者的回報：${task.last_report}`,
          task.id,
        )
      }
    }

    const merge = (task: MockTeamTask) => () => {
      task.state = 'merged'
      task.merge_sha = `${Math.random().toString(16).slice(2, 9)}f0`
      this.emitTask(task)
      this.teamEvent(t.id, 'merge', { branch: task.branch, result: 'ok', sha: task.merge_sha }, { task_id: task.id })
      if (pmBot) {
        this.teamRelay(t, DAEMON_SENDER, pmBot.id, `t${task.seq}「${task.title}」已合併進 ${t.branch}（${task.merge_sha}）。`, task.id)
      }
    }

    tasks.forEach((task, i) => {
      steps.push(report(task, false))
      if (rev && i === reworkAt) {
        steps.push(() => {
          task.round += 1
          t.usage.review_rounds_total += 1
          task.last_verdict = 'request_changes：錯誤路徑沒有測試，且 LEFT JOIN 後的 NULL 名稱會顯示成 undefined'
          this.teamReply(
            t,
            rev.id,
            `看過 \`git diff ${t.branch}...HEAD\`，主要邏輯正確，但有兩點必須修：\n\n1. 刪除的 bot 名稱會變成 \`undefined\`\n2. 少了錯誤路徑的測試\n\n` +
              amTeam({
                action: 'verdict',
                result: 'request_changes',
                summary: '主要邏輯正確，但顯示名與測試要補',
                must_fix: ['daemon/src/group.rs: 名稱 fallback', 'daemon/tests: 錯誤路徑'],
              }),
          )
          task.state = 'changes_requested'
          this.emitTask(task)
          this.teamRelay(
            t,
            rev.id,
            task.worker_bot_id,
            `Reviewer 打回（第 ${task.round} 回）：${task.last_verdict}。在同一分支繼續，完成後再 report。`,
            task.id,
          )
          task.state = 'working'
          this.emitTask(task)
        })
        steps.push(report(task, true))
      }
      if (rev) {
        steps.push(() => {
          task.last_verdict = 'approve：符合 issue 描述，測試涵蓋錯誤路徑'
          this.teamReply(
            t,
            rev.id,
            `這次沒問題了。\n\n${amTeam({ action: 'verdict', result: 'approve', summary: task.last_verdict })}`,
          )
          task.state = 'merging'
          this.emitTask(task)
        })
      }
      steps.push(merge(task))
    })

    steps.push(() => {
      t.summary = `修正群組合併時間軸在成員被刪除後遺失歷史的問題，並補上回歸測試。共 ${tasks.length} 件 task 全數合併。`
      if (pmBot) {
        this.teamReply(t, pmBot.id, `所有 task 都已合併。\n\n${t.summary}\n\n${amTeam({ action: 'done', summary: t.summary })}`)
      }
      this.setPhase(t, 'finishing')
    })

    steps.push(() => {
      if (t.deliver === 'pr') {
        const p = this.projects.find((x) => x.id === t.project_id)
        t.pr_url = `${p?.github?.url ?? 'https://github.com/me/repo'}/pull/${t.issue_number + 100}`
        this.teamEvent(t.id, 'note', { text: `已 push ${t.branch} 並開 PR ${t.pr_url}` })
      } else {
        this.teamEvent(t.id, 'note', { text: `整合分支 ${t.branch} 已留在 repo（未 push）。` })
      }
      const current = t.issues.find((issue) => issue.state === 'working')
      if (current) {
        current.state = 'done'
        current.summary = t.summary
        current.pr_url = t.pr_url
        current.ended_at = now()
      }
      this.setPhase(t, 'done')
      for (const m of t.members) this.stop(m.bot_id)
    })

    this.teamSteps.set(t.id, steps)
  }

  private scheduleTeam(t: MockTeam, delay = 1600) {
    const prev = this.teamTimers.get(t.id)
    if (prev) clearTimeout(prev)
    this.teamTimers.set(
      t.id,
      setTimeout(() => this.teamTick(t.id), delay),
    )
  }

  private teamTick(teamId: string) {
    this.teamTimers.delete(teamId)
    const t = this.teams.find((x) => x.id === teamId)
    if (!t) return
    if (t.phase === 'paused' || t.phase === 'aborting' || t.phase === 'aborted' || t.phase === 'done' || t.phase === 'failed') {
      return
    }
    const steps = this.teamSteps.get(teamId) ?? []
    const step = steps.shift()
    if (!step) return
    t.usage.elapsed_min += 1
    step()
    // §4.5 預算：轉送次數到頂就停下來（可加碼後 resume）。
    // `step()` can advance the phase to a terminal state, but TypeScript cannot see that
    // mutation through the callback, so widen the narrowed phase before checking it.
    if (t.usage.relays >= t.budget.max_relays && (t.phase as TeamPhase) !== 'done') {
      this.pauseWith(t, 'budget_relays')
      return
    }
    if (steps.length > 0) this.scheduleTeam(t)
  }

  private pauseWith(t: MockTeam, reason: string) {
    if (t.phase === 'paused') return
    t.resume_phase = t.phase
    this.setPhase(t, 'paused', reason)
  }

  private pauseTeam(id: string) {
    const t = this.team(id)
    if (t.phase === 'done' || t.phase === 'aborted' || t.phase === 'failed') {
      throw new ApiError(409, { error: 'conflict', reason: 'team 已在終態' }, 'conflict')
    }
    const timer = this.teamTimers.get(id)
    if (timer) clearTimeout(timer)
    this.teamTimers.delete(id)
    this.pauseWith(t, 'user')
    return {}
  }

  private resumeTeam(id: string) {
    const t = this.team(id)
    if (t.phase !== 'paused') throw new ApiError(409, { error: 'conflict', reason: 'team 不在 paused' }, 'conflict')
    if (t.usage.relays >= t.budget.max_relays) {
      throw new ApiError(409, { error: 'conflict', reason: '轉送次數仍已用盡，請先加碼 max_relays' }, 'conflict')
    }
    t.phase = t.resume_phase ?? 'working'
    t.pause_reason = null
    t.resume_phase = null
    this.teamEvent(t.id, 'phase', { from: 'paused', to: t.phase, reason: 'resume' })
    this.emitTeam(t)
    this.scheduleTeam(t, 800)
    return {}
  }

  private approveTeam(id: string) {
    const t = this.team(id)
    if (!(t.pause_reason ?? '').startsWith('gate:')) {
      throw new ApiError(409, { error: 'conflict', reason: 'team 不在 supervised 閘門上' }, 'conflict')
    }
    return this.resumeTeam(id)
  }

  private abortTeam(id: string) {
    const t = this.team(id)
    const timer = this.teamTimers.get(id)
    if (timer) clearTimeout(timer)
    this.teamTimers.delete(id)
    this.teamSteps.set(id, [])
    this.setPhase(t, 'aborting')
    setTimeout(() => {
      for (const m of t.members) this.stop(m.bot_id)
      this.setPhase(t, 'aborted')
    }, 700)
    return {}
  }

  /** `POST /api/teams/:id/issues` — exercise the done-team continuation in the manual mock. */
  private addTeamIssues(id: string, b: Rec) {
    const t = this.team(id)
    if (t.phase === 'aborted' || t.phase === 'failed') {
      throw new ApiError(409, { error: 'conflict', reason: 'team is finished', phase: t.phase }, 'conflict')
    }
    const raw = Array.isArray(b.issue_numbers) ? b.issue_numbers : b.issue_number === undefined ? [] : [b.issue_number]
    const numbers = raw.map((number) => Number(number)).filter((number) => Number.isInteger(number) && number > 0)
    if (!numbers.length) throw new ApiError(400, { error: 'bad_request', message: 'issue_numbers must not be empty' }, 'bad request')
    if (t.issues.length + numbers.length > 20) {
      throw new ApiError(400, { error: 'bad_request', message: 'at most 20 issues per team' }, 'bad request')
    }
    // SPEC-team §2.3：只有還在佇列上（`queued` / `working`）的同號 issue 才擋；做完 / 失敗 /
    // 略過的可以再排一次（新的一列，舊的留著當紀錄）。
    const duplicate = numbers.find(
      (number, index) =>
        numbers.slice(0, index).includes(number) ||
        t.issues.some(
          (issue) => issue.issue_number === number && (issue.state === 'queued' || issue.state === 'working'),
        ),
    )
    if (duplicate !== undefined) {
      throw new ApiError(409, { error: 'conflict', reason: 'issue already queued', issue_number: duplicate }, 'conflict')
    }
    if (t.phase === 'done' && !t.members.some((member) => member.role === 'pm' && this.bots.some((bot) => bot.id === member.bot_id))) {
      throw new ApiError(409, { error: 'conflict', reason: 'team is cleaned up', phase: t.phase }, 'conflict')
    }

    const p = this.projects.find((project) => project.id === t.project_id)
    const added: MockTeamIssue[] = []
    for (const number of numbers) {
      const source = ISSUES.find((issue) => issue.number === number)
      if (!source) throw new ApiError(502, { error: 'upstream', message: `gh: issue #${number} not found` }, 'upstream')
      const entry: MockTeamIssue = {
        id: ulid('issue'),
        seq: (t.issues.at(-1)?.seq ?? 0) + 1,
        issue_number: number,
        issue_title: String(source.title),
        issue_url: `${p?.github?.url ?? 'https://github.com/me/repo'}/issues/${number}`,
        state: 'queued',
        branch: null,
        summary: null,
        pr_url: null,
        issue_closed_at: null,
        fail_reason: null,
        started_at: null,
        ended_at: null,
      }
      t.issues.push(entry)
      added.push(entry)
    }
    this.teamEvent(id, 'note', { action: 'issues_queued', issue_numbers: numbers })

    if (t.phase === 'done') {
      this.teamEvent(id, 'note', { action: 'team_reopened', by: 'user', issue_numbers: numbers, from_phase: 'done' })
      const pm = this.memberBot(t, 'pm')
      if (pm) {
        this.teamEvent(id, 'note', { action: 'member_context_lost', bot: pm.name, role: 'pm', why: 'no_session_id' }, { to_bot_id: pm.id })
        if (!this.activeRun(pm.id)) this.start(pm.id)
      }
      const rev = this.memberBot(t, 'reviewer')
      if (rev && !this.activeRun(rev.id)) this.start(rev.id)
      t.ended_at = null
      this.setPhase(t, 'starting', 'reopen')
      setTimeout(() => {
        if (t.phase !== 'starting') return
        const next = t.issues.find((issue) => issue.state === 'queued')
        if (!next) return
        next.state = 'working'
        next.started_at = now()
        next.branch = `team/i${next.issue_number}-${t.id.slice(-6).toLowerCase()}`
        t.issue_number = next.issue_number
        t.issue_title = next.issue_title
        t.issue_url = next.issue_url
        t.branch = next.branch
        t.summary = null
        t.pr_url = null
        t.issue_closed_at = null
        this.teamEvent(id, 'note', { action: 'issue_started', issue_number: next.issue_number, seq: next.seq, branch: next.branch })
        this.setPhase(t, 'planning')
        const currentPm = this.memberBot(t, 'pm')
        if (currentPm) {
          this.teamRelay(t, DAEMON_SENDER, currentPm.id, `換下一個 issue：#${next.issue_number}「${next.issue_title}」。你是重新啟動的 PM，先前的對話不在了；先讀 \`.agents-manager/team/TEAM.md\` 與 \`ISSUE.md\` 再派工。`)
        }
        this.emitTeam(t)
      }, 1000)
    }
    this.emitTeam(t)
    return { issues: t.issues.map((issue) => ({ ...issue })) }
  }

  /**
   * `POST /api/teams/:id/close-issue`（SPEC-team §10.7）。
   *
   * 守則跟 daemon 一樣、也只有這兩條：**只有 `done` 的 team**、**只關一次**。mock 沒有真的
   * GitHub，所以只把 `issue_closed_at` 記下來並發一則 `issue_closed` 事件——驗收要看的是
   * 「按鈕只在完成後出現、按過就變成已關閉」這件事。
   */
  private closeTeamIssue(id: string, b: Rec) {
    const t = this.team(id)
    const target = typeof b.issue_id === 'string'
      ? t.issues.find((issue) => issue.id === b.issue_id)
      : t.issues.find((issue) => issue.issue_number === t.issue_number)
    if (!target) throw new ApiError(404, { error: 'not_found', what: 'issue' }, 'not found')
    if (target.state !== 'done') {
      throw new ApiError(409, { error: 'conflict', state: target.state, phase: t.phase }, 'conflict')
    }
    if (target.issue_closed_at) {
      throw new ApiError(409, { error: 'conflict', reason: 'issue 已經從這個 team 關過了' }, 'conflict')
    }
    target.issue_closed_at = new Date().toISOString()
    if (target.issue_number === t.issue_number) t.issue_closed_at = target.issue_closed_at
    this.teamEvent(id, 'note', {
      action: 'issue_closed',
      by: 'user',
      number: target.issue_number,
      url: target.issue_url,
      already_closed: false,
      comment: typeof b.comment === 'string' ? b.comment : null,
    })
    this.emitTeam(t)
    return { number: target.issue_number, url: target.issue_url, state: 'CLOSED', already_closed: false }
  }

  private cleanupTeam(id: string) {
    const t = this.team(id)
    if (t.phase !== 'done' && t.phase !== 'aborted' && t.phase !== 'failed') {
      throw new ApiError(409, { error: 'conflict', reason: 'team 尚未進入終態' }, 'conflict')
    }
    for (const m of t.members) {
      this.bots = this.bots.filter((b) => b.id !== m.bot_id)
      this.emit('bot_changed', { bot_id: m.bot_id, deleted: true })
    }
    this.teams = this.teams.filter((x) => x.id !== id)
    this.teamTasks = this.teamTasks.filter((x) => x.team_id !== id)
    this.teamEvents = this.teamEvents.filter((x) => x.team_id !== id)
    this.teamSteps.delete(id)
    this.emit('project_changed', { project_id: t.project_id })
    return {}
  }

  /**
   * `DELETE /api/teams/:id?branches=keep|delete`（SPEC-team §6.5a）。
   *
   * 與 `cleanup` 的差別：**任何 phase 都可以刪**（非終態時等於先 abort 再刪），而且連
   * `teams` / `team_tasks` / `team_events` 的紀錄一起移除。成員 bot 走 `deleteBot`，
   * 所以**對話訊息保留**。`branches=delete` 在 mock 裡沒有真的 repo 可以動，只把「分支已刪」
   * 記進 console，讓兩條路徑在驗收時分得出來。不存在 → `this.team()` 丟 404（冪等）。
   */
  private deleteTeam(id: string, branches: 'keep' | 'delete') {
    const t = this.team(id)
    const timer = this.teamTimers.get(id)
    if (timer) clearTimeout(timer)
    this.teamTimers.delete(id)
    this.teamSteps.delete(id)
    // 刪除中：真 daemon 會先推這一則（phase `deleting`）。
    this.emit('team_changed', { team_id: id, project_id: t.project_id, phase: 'deleting' })
    for (const m of t.members) {
      if (this.bots.some((b) => b.id === m.bot_id)) this.deleteBot(m.bot_id)
    }
    this.teams = this.teams.filter((x) => x.id !== id)
    this.teamTasks = this.teamTasks.filter((x) => x.team_id !== id)
    this.teamEvents = this.teamEvents.filter((x) => x.team_id !== id)
    if (branches === 'delete') {
      console.info(`[mock] git branch -D ${t.branch}（含所有 task 分支）；遠端分支不動`)
    }
    this.emit('team_changed', { team_id: id, project_id: t.project_id, deleted: true })
    return {}
  }

  private patchTeam(id: string, b: Rec) {
    const t = this.team(id)
    if (t.phase === 'done' || t.phase === 'aborted' || t.phase === 'failed') {
      throw new ApiError(409, { error: 'conflict', reason: 'team 已在終態' }, 'conflict')
    }
    if (b.budget && typeof b.budget === 'object') {
      for (const [k, v] of Object.entries(b.budget as Rec)) {
        if (k in t.budget && typeof v === 'number') (t.budget as unknown as Rec)[k] = v
      }
    }
    if (b.supervised !== undefined) t.supervised = b.supervised === true
    if (b.deliver === 'pr' || b.deliver === 'branch') t.deliver = b.deliver
    this.emitTeam(t)
    return {}
  }

  private teamSay(id: string, b: Rec) {
    const t = this.team(id)
    const text = String(b.text ?? '').trim()
    if (!text) throw new ApiError(400, { error: 'bad_request', message: 'text 不可為空' }, 'bad request')
    const to = String(b.to ?? 'pm')
    const target =
      t.members.find((m) => m.bot_id === to)?.bot_id ?? this.memberBot(t, to === 'pm' ? 'pm' : 'worker')?.id ?? null
    if (!target) throw new ApiError(404, { error: 'not_found', what: 'member' }, 'not found')
    // §5.4：使用者插話記 `kind:user`，不計 relay 預算。
    this.addMessage({
      conversation_id: this.conv(target),
      turn_id: null,
      bot_id: target,
      role: 'user',
      content: text,
      source: 'web',
      incomplete: 0,
      team_id: t.id,
    })
    this.teamEvent(t.id, 'user', { text_excerpt: text.slice(0, 120) }, { to_bot_id: target })
    const run = this.activeRun(target)
    if (run && run.state === 'running') {
      run.agent_status = 'working'
      this.emitBotStatus(target)
    }
    setTimeout(() => {
      this.teamReply(t, target, '收到，我會把這點納入下一輪的判斷。（mock）')
    }, 1800)
    return { team_id: t.id, sent: [{ bot_id: target, delivery: 'ok' }] }
  }

  private teamAnswer(id: string, b: Rec) {
    const t = this.team(id)
    const pm = this.memberBot(t, 'pm')
    this.teamSay(id, { text: b.text, to: pm?.id ?? 'pm' })
    if (t.phase === 'paused') this.resumeTeam(id)
    return {}
  }

  private decideTask(teamId: string, taskId: string, b: Rec) {
    const t = this.team(teamId)
    const task = this.teamTasks.find((x) => x.id === taskId && x.team_id === teamId)
    if (!task) throw new ApiError(404, { error: 'not_found', what: 'task' }, 'not found')
    if (task.state !== 'exhausted' && task.state !== 'blocked_by_worker' && task.state !== 'rebasing') {
      throw new ApiError(409, { error: 'conflict', reason: `task 狀態 ${task.state} 不接受 decide` }, 'conflict')
    }
    const action = String(b.action ?? '')
    if (action === 'skip') task.state = 'skipped'
    else if (action === 'force_merge') task.state = 'merging'
    else task.state = 'working'
    this.emitTask(task)
    this.teamEvent(teamId, 'note', { text: `使用者決定：${action}`, task_id: taskId }, { task_id: taskId })
    if (t.phase === 'paused') this.resumeTeam(teamId)
    return {}
  }

  // -------------------------------------------------------------- dev helpers

  forceResync() {
    if (this.handlers && this.socketOpen) this.handlers.onFrame({ type: 'resync' })
  }

  dropSocket() {
    if (!this.handlers) return
    const h = this.handlers
    this.socketOpen = false
    h.onStatus('closed')
    setTimeout(() => {
      if (this.handlers !== h) return
      this.socketOpen = true
      h.onStatus('open')
      h.onFrame({ type: 'resync' })
    }, 1500)
  }

  setConnected(v: boolean) {
    this.connected = v
    // SPEC §11.6: `daemon_status` carries `herdr_connected` + a per-host map. `connected`
    // is kept for the pre-§11 shape.
    this.emit('daemon_status', { herdr_connected: v, connected: v, default_connected: false, hosts: this.hostMap() })
    for (const b of this.bots) this.emitBotStatus(b.id)
  }

  botIdByName(name: string): string | undefined {
    return this.bots.find((b) => b.name === name)?.id
  }

  /** Dev helper: 目前的 team id（最新的排最後）。 */
  teamIds(): string[] {
    return this.teams.map((t) => t.id)
  }

  /**
   * Dev helper: 模擬舊 daemon —— `/api/teams*` 全部落到「查無此路由」的裸 404，
   * `GET /state` 也不再有 `projects[].teams`。用來驗前端的優雅退回。
   */
  /**
   * Dev helper: 預設分頁裡塞幾個外來 pane。數字越大，bot 的 pane 越窄
   * （`WORKSPACE_COLUMNS / (n + 共用分頁的 run 數)`），用來驗窄 pane 警示。
   */
  setForeignPanes(n: number) {
    this.foreignPanes = Math.max(0, Math.floor(n))
  }

  /** Dev helper: 模擬舊 daemon —— `POST /bots/:id/pane/move-to-tab` 落到查無此路由的 404。 */
  setPaneMoveSupported(on: boolean) {
    this.paneMoveDisabled = !on
  }

  /** Dev helper: 模擬舊 daemon —— `/api/hosts/:name/shells*` 全部落到查無此路由的裸 404。 */
  setHostShellsSupported(on: boolean) {
    this.hostShellsDisabled = !on
  }

  setTeamsSupported(on: boolean) {
    this.teamsDisabled = !on
    if (!on) {
      for (const id of [...this.teamTimers.keys()]) {
        clearTimeout(this.teamTimers.get(id)!)
        this.teamTimers.delete(id)
      }
      this.teamSteps.clear()
    }
    this.emit('project_changed', {})
  }

  /**
   * Dev helper: 用任意原因把某個 team 停下來，驗 paused 橫幅
   * （例：`__amMock.teamPause('review_exhausted')`；省略 id = 最後一個 team）。
   */
  forceTeamPause(reason: string, teamId?: string) {
    const t = teamId ? this.teams.find((x) => x.id === teamId) : this.teams[this.teams.length - 1]
    if (!t) return
    const timer = this.teamTimers.get(t.id)
    if (timer) clearTimeout(timer)
    this.teamTimers.delete(t.id)
    // 已經是 paused 時也要能換原因（驗各種橫幅文案用）。
    if (t.phase === 'paused') t.phase = t.resume_phase ?? 'working'
    this.pauseWith(t, reason)
  }

  /** Dev helper: 把某個 team 的第一件未完成 task 推進「需要你」欄。 */
  forceTeamNeedsUser(state: 'exhausted' | 'blocked_by_worker' = 'exhausted', teamId?: string) {
    const t = teamId ? this.teams.find((x) => x.id === teamId) : this.teams[this.teams.length - 1]
    if (!t) return
    const task = this.teamTasks.find(
      (x) => x.team_id === t.id && x.state !== 'merged' && x.state !== 'skipped' && x.state !== 'failed',
    )
    if (!task) return
    task.state = state
    this.emitTask(task)
    this.forceTeamPause(state === 'exhausted' ? 'review_exhausted' : 'pm_abort', t.id)
  }
}

function sleep(ms: number) {
  return new Promise<void>((r) => setTimeout(r, ms))
}

function installDevHelpers(mock: MockTransport) {
  ;(globalThis as unknown as Rec).__amMock = {
    resync: () => mock.forceResync(),
    dropSocket: () => mock.dropSocket(),
    block: (botIdOrName: string) => mock.enterBlocked(mock.botIdByName(botIdOrName) ?? botIdOrName),
    disconnect: () => mock.setConnected(false),
    reconnect: () => mock.setConnected(true),
    hostDown: (name: string) => mock.setHostConnected(name, false),
    hostUp: (name: string) => mock.setHostConnected(name, true),
    hosts: () => mock.hostNames(),
    // SPEC-team
    teams: () => mock.teamIds(),
    teamsOff: () => mock.setTeamsSupported(false),
    teamsOn: () => mock.setTeamsSupported(true),
    teamPause: (reason = 'budget_relays', teamId?: string) => mock.forceTeamPause(reason, teamId),
    teamNeedsUser: (state: 'exhausted' | 'blocked_by_worker' = 'exhausted', teamId?: string) =>
      mock.forceTeamNeedsUser(state, teamId),
    // 窄 pane / 移到自己的分頁
    paneSqueeze: (n = 5) => mock.setForeignPanes(n),
    paneMoveOff: () => mock.setPaneMoveSupported(false),
    paneMoveOn: () => mock.setPaneMoveSupported(true),
    // 主機 shell
    hostShellsOff: () => mock.setHostShellsSupported(false),
    hostShellsOn: () => mock.setHostShellsSupported(true),
  }
}
