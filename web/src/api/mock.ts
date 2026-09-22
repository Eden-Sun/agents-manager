/**
 * In-memory daemon simulator (`VITE_MOCK=1`) for SPEC §7 REST + §7.3 WS events.
 * Prompt keywords (`blocked`/`fallback`/`slow`) and `window.__amMock` helpers: see docs/FRONTEND.md.
 */

import { parseMentions } from './mentions'
import { BOT_NAME_HINT, isValidBotName } from '../lib/botName'
import { ApiError, BOT_KINDS } from './types'
import type { BotKind } from './types'
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

/** herdr workspace 總欄數（2026-09-06 實測 185）；同分頁 pane 平分，獨佔分頁全拿。 */
const WORKSPACE_COLUMNS = 185

interface MockRun {
  id: string
  bot_id: string
  state: 'starting' | 'running' | 'stopping' | 'stopped' | 'exited'
  agent_status: 'idle' | 'working' | 'blocked' | 'unknown'
  workspace_id: string | null
  pane_id: string | null
  /** false = 跟其他 pane 擠預設分頁。 */
  own_tab: boolean
  adopted: number
  native_session_id: string | null
  transcript_path: string | null
  /** claude statusLine hook payload（見 normalize.toStatusInfo）。 */
  status: Record<string, unknown> | null
  /** 被終端寬度壓縮過的原文，當 tooltip / fallback。 */
  status_line: string | null
  /** null = 沒有待重啟套用的新版。 */
  update_notice: string | null
  /** 上一回合被 API 斷線截斷時的原文；null = 正常收尾。 */
  turn_error: string | null
  /** SPEC §4.4a：run 實際跑的值，啟動時複製；PATCH 不動它，只有 slash 指令當場套用（grok/claude）才跟著改。 */
  runtime_model: string | null
  runtime_effort: string | null
  runtime_fast: boolean
  /** 空字串＝本機預設帳號；null＝不知道。 */
  runtime_identity: string | null
  started_at: string
  ended_at: string | null
  /** `agent_status` 最後一次真的改變的時間，只有 `setAgentStatus` 會動它（issue #93）。 */
  agent_status_since: string | null
}

/** 同值重寫不算改變：mock 也要跟 daemon 的 trigger 同一套規則，不然本地開發永遠測不到「跑了多久」。 */
function setAgentStatus(run: MockRun, status: MockRun['agent_status']) {
  if (run.agent_status === status) return
  run.agent_status = status
  run.agent_status_since = now()
}

/** Real-session values so the status bar is exercised at a realistic width. */
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
  status: 'queued' | 'in_flight' | 'completed' | 'completed_fallback' | 'failed'
  delivery: 'pending' | 'ok' | 'unknown' | 'failed'
  client_request_id: string | null
  created_at: string
  completed_at: string | null
  /** issue #122：bot 沒在跑時收下、等它起來的那一則。 */
  awaits_start?: number
  start_error?: string | null
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
  /** SPEC §13：null = 一般訊息 */
  group_id: string | null
  /** 只存 metadata，位元組在 `blobs`。 */
  attachments_json: string | null
  /** null = 使用者 / daemon 自己。 */
  relay_from: string | null
  created_at: string
}

interface MockBot {
  id: string
  project_id: string
  name: string
  kind: BotKind
  /** API.md v3.3：null = 不帶 `--model` */
  model: string | null
  effort: string | null
  fast: number
  persona: string | null
  /** claude only（issue #213）；null = 沒設＝`claude-md`。 */
  instruction_files: string | null
  args_json: string
  autostart: number
  inject_hooks: number
  auto_approve: number
  identity: string | null
  env_json: string
  managed_by: 'user' | 'child'
  /** child 才有：母 bot 的 id（側欄縮排在它底下）。 */
  parent_bot_id?: string
  cwd: string | null
  /** 使用者釘選（`PATCH {primary}`）；省略 = 沒釘。 */
  is_primary?: number
  primary_position?: number
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
  /** null = not a GitHub project */
  github: { owner: string; repo: string; url: string } | null
  created_at: string
}

const ISSUES: Rec[] = [
  { number: 42, title: '群組聊天：刪掉的 bot 歷史不再出現在合併時間軸', state: 'open', labels: [{ name: 'bug', color: 'd73a4a' }, { name: 'group-chat', color: '0e8a16' }], author: 'edansun', updated_at: inHours(-3), body: '重現步驟：\n1. 在群組視圖送 `@all` \n2. 刪掉其中一個 bot\n3. 重新整理\n\n預期：歷史仍在；實際：只剩存活 bot 的訊息。\n\n相關：`GET /projects/:id/messages` 只合併現存 bot。' },
  { number: 41, title: '即時輸出前幾幀是 TUI 雜訊（✢ Improvising…）', state: 'open', labels: [{ name: 'daemon', color: '1d76db' }, { name: 'polish', color: 'fbca04' }], author: 'edansun', updated_at: inHours(-9), body: '`turn_progress` 的前 1–2 幀會帶 Claude Code 的 spinner 文字，應在 daemon 端過濾。' },
  { number: 40, title: 'Bot 設定：codex 模型 / effort / fast 從 `GET /api/models` 取', state: 'open', labels: [{ name: 'enhancement', color: 'a2eeef' }, { name: 'web', color: '5319e7' }], author: 'edansun', updated_at: inHours(-20), body: '目前是靜態清單。改為 API 驅動，失敗退回靜態清單。' },
  { number: 39, title: '主機面板顯示三個 kind 的工具偵測徽章', state: 'open', labels: [{ name: 'enhancement', color: 'a2eeef' }], author: 'm4p-bot', updated_at: inHours(-30), body: '已安裝 ✓ / 未安裝 ✗ / 未登入 !，附「安裝」按鈕。' },
  { number: 38, title: '額度徽章：低於 20% 用警示色', state: 'open', labels: [{ name: 'web', color: '5319e7' }, { name: 'good first issue', color: '7057ff' }], author: 'edansun', updated_at: inHours(-50), body: '剩餘 = 100 − used_pct。' },
  { number: 37, title: '長訊息不要預先收合', state: 'closed', labels: [{ name: 'web', color: '5319e7' }], author: 'edansun', updated_at: inHours(-60), body: '已在 v3.9 UI polish 移除 isLong / clamped。' },
  { number: 35, title: 'hash 式 herdr agent name', state: 'closed', labels: [{ name: 'daemon', color: '1d76db' }], author: 'edansun', updated_at: inHours(-100), body: '`<project slug>-<bot id 尾 6 碼>`。' },
  { number: 33, title: 'grok kind：hook 不走 argv', state: 'closed', labels: [{ name: 'daemon', color: '1d76db' }, { name: 'grok', color: '000000' }], author: 'edansun', updated_at: inHours(-140), body: '改寫入 `<GROK_HOME>/hooks/agents-manager.json`。' },
]

/** SPEC §11.2 `[[hosts]]` + runtime connection state. */
interface MockHost {
  name: string
  ssh: string
  ssh_port: number
  herdr_session: string
  remote_path: string
  connected: boolean
  error: string | null
  tools: Record<BotKind, MockTool>
  /** Keyed by identity name. */
  identities: Record<string, MockIdentityStatus>
}

interface MockTool {
  installed: boolean
  path: string | null
  version: string | null
  logged_in: boolean | null
}

interface MockShell {
  host: string
  pane_id: string
  tab_id: string
  workspace_id: string
  cwd: string
  created_at: string
  lines: string[]
  /** 提示符後還沒按 Enter 的字。 */
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

interface MockIdentityStatus {
  name: string
  kind: BotKind
  logged_in: boolean | null
  reason?: string
  account?: string
  plan?: string
  /** `shell` = 那台主機 zshrc 的 `ccN`（SPEC §16）。 */
  source: 'config' | 'shell'
  /** shell 來源的 `CLAUDE_CONFIG_DIR`（預設帳號沒有）。 */
  config_dir?: string
}

const TOOLS_ALL_OK: Record<BotKind, MockTool> = {
  claude: { installed: true, path: '/opt/homebrew/bin/claude', version: '2.1.40', logged_in: true },
  codex: { installed: true, path: '/opt/homebrew/bin/codex', version: '0.68.0', logged_in: true },
  grok: { installed: true, path: '/Users/me/.local/bin/grok', version: '1.0.13', logged_in: null },
}

/** Catalogue as the CLIs reported on 2026-09-06. claude 2.1 effort 每個 alias 同一組（codex 是 per-model）。 */
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
    { id: 'grok-4.7', display_name: 'Grok 4.7', description: 'grok CLI 預設', is_default: true, default_effort: 'high', efforts: ['low', 'medium', 'high', 'xhigh'], service_tiers: [] },
    { id: 'grok-4.7-build-fast', display_name: 'Grok 4.7 Fast', description: '較快變體', is_default: false, default_effort: 'high', efforts: ['low', 'medium', 'high', 'xhigh'], service_tiers: [] },
    { id: 'grok-4.6', display_name: 'Grok 4.6', description: '上一代', is_default: false, default_effort: 'high', efforts: ['low', 'medium', 'high', 'xhigh'], service_tiers: [] },
    { id: 'grok-4.5', display_name: 'Grok 4.5', description: '上一代', is_default: false, default_effort: 'high', efforts: ['low', 'medium', 'high'], service_tiers: [] },
  ],
}

/**
 * claude `default_effort` 來自帳號 settings.json 的全域＋per-model `effortLevel`（SPEC §17.1），數字照真機實測。
 * 都沒設時內建預設是 `high`（官方文件；2026-09-07 乾淨 cc2 實測 `Sonnet 5 with high effort`）。
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
  // Fenced one-liner (what `echo 1` really produced).
  '```\n1\n```',
]

interface MockMission {
  id: string
  project_id: string
  client_request_id: string
  text: string
  delivery_mode: 'push_main' | 'pr'
  executor_kind: BotKind
  on_5h_limit: 'wait' | 'switch'
  max_rounds: number
  rounds_used: number
  paused_reason: string | null
  paused_detail: string | null
  result_summary: string | null
  /** 非 null ＝ 這筆是某個成果的續作。 */
  parent_mission_id: string | null
  created_at: string
  updated_at: string
  completed_at: string | null
  cancelled_at: string | null
}

interface MockMissionAssignment {
  id: string
  mission_id: string
  role: 'executor' | 'reviewer' | 'verifier'
  status: string
  target_bot_id: string | null
  turn_status: string | null
  turn_error: string | null
  follow_up_of: string | null
  resume_at: string | null
  created_at: string
  completed_at: string | null
}

interface MockMissionEvent {
  id: string
  mission_id: string
  kind: string
  text: string
  relay_from: string | null
  payload: Rec | null
  reply_to: string | null
  client_request_id: string | null
  created_at: string
}

export class MockTransport implements Transport {
  readonly mock = true

  private hosts: MockHost[] = []
  /** grok missing, codex not logged in — exercises the hint UI. */
  private localTools: Record<BotKind, MockTool> = {
    claude: { ...TOOLS_ALL_OK.claude },
    codex: { ...TOOLS_ALL_OK.codex, logged_in: false },
    grok: { installed: false, path: null, version: null, logged_in: null },
  }
  /** cc1 未登入演「未登入」標記；cc2 來自 zshrc alias（SPEC §16），演兩種來源的差別。 */
  /** 停用名單，鍵是 `host|kind|name`（真 daemon 存在 `identity_prefs`）。 */
  private disabledIdentities: string[] = []

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
  /** SPEC §14：裸 key 是本機，遠端加 `<host>/` 前綴（`seedHostQuota()` 故意給不同數字）。 */
  private quota: Record<string, Rec | null> = {
    claude: { five_hour: { used_pct: 18, resets_at: inHours(2.4) }, seven_day: { used_pct: 40, resets_at: inHours(70) }, plan: 'Max 20x', updated_at: now(), host: 'local' },
    'claude:cc1': { five_hour: { used_pct: 85, resets_at: inHours(1.1) }, seven_day: { used_pct: 30, resets_at: inHours(120) }, plan: 'Pro', updated_at: now(), host: 'local' },
    // zshrc 身份也有額度列（SPEC §16）。
    'claude:cc2': { five_hour: { used_pct: 24, resets_at: inHours(3.8) }, seven_day: { used_pct: 51, resets_at: inHours(88) }, plan: 'Pro', updated_at: now(), host: 'local' },
    // 重置券（codex `rateLimitResetCredits`）：固定給一張讓明細有東西可看。
    codex: {
      five_hour: { used_pct: 63, resets_at: inHours(3.2) },
      seven_day: { used_pct: 88, resets_at: inHours(41) },
      reset_credits: { available: 1, title: 'Full reset (Weekly + 5 hr)', expires_at: inHours(720) },
      plan: 'Plus',
      updated_at: now(),
      host: 'local',
    },
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
  /** 群組任務：只做狀態與事件，不真的派 bot。 */
  private missions: MockMission[] = []
  private missionEvents: MockMissionEvent[] = []
  private missionAssignments: MockMissionAssignment[] = []
  private conversations = new Map<string, string>()
  private blobs = new Map<string, Blob>()

  /** 模擬舊 daemon 沒有 `/api/missions`（`__amMock.missionsOff()`）。 */
  private missionsDisabled = false

  /** 下一個符合的請求回指定的錯誤（`__amMock.failNext`）：手動看錯誤路徑、截圖用，用過一次就拿掉。 */
  private faults: { method: HttpMethod; pattern: RegExp; status: number; body: Rec }[] = []
  failNext(method: HttpMethod, pattern: string, status: number, body: Rec = {}) {
    this.faults.push({ method, pattern: new RegExp(pattern), status, body })
  }

  /** 面板自己開的 shell（daemon 的 `app.host_shells`）。key = `<host>/<pane_id>`。 */
  private shells = new Map<string, MockShell>()
  /**
   * 被 trace 的 pane 的畫面緩衝。**不放進 `shells`**：放進去就會被 `GET …/shells` 列成「自己開的」、
   * 之後 `shell()` 直接放行不再看唯讀——daemon 那邊這兩份白名單一直是分開的（第二輪 review M6）。
   */
  private tracedScreens = new Map<string, MockShell>()
  private shellSeq = 0

  /** 新遠端主機預設同 m4p 實測：active token 失效、另有可切帳號。 */
  private gh = new Map<string, MockGh>()

  /** 預設分頁裡非 bot 的 pane 數；預設夠擠以演窄 pane 警示（`__amMock.paneSqueeze(n)` 調整）。 */
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
    this.seedMissions(p.id)
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-claude',
      kind: 'claude',
      model: null,
      effort: null,
      fast: 0,
      persona: '你是 agents-manager 的 PM。回覆用繁體中文，先給結論再列理由；改動前先說明影響範圍。',
      instruction_files: null,
      args_json: '[]',
      autostart: 1,
      inject_hooks: 1,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      is_primary: 1,
      managed_by: 'user',
      cwd: null,
      created_at: now(),
    })
    // 第二顆 claude：SPEC §6.9 批次重啟要一閒一忙才演得出跳過規則。
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-claude-2',
      kind: 'claude',
      model: null,
      effort: null,
      fast: 0,
      persona: null,
      instruction_files: null,
      args_json: '[]',
      autostart: 0,
      inject_hooks: 1,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      is_primary: 1,
      managed_by: 'user',
      cwd: null,
      created_at: now(),
    })
    // 母 agent 自己開的子 agent（managed_by=child）：選單要有「升級成頂層」。
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-claude-kid',
      kind: 'claude',
      model: null,
      effort: null,
      fast: 0,
      persona: null,
      instruction_files: null,
      args_json: '[]',
      autostart: 0,
      inject_hooks: 0,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      managed_by: 'child',
      parent_bot_id: this.bots[0].id,
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
      instruction_files: null,
      args_json: '[]',
      autostart: 0,
      inject_hooks: 1,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      is_primary: 1,
      managed_by: 'user',
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
      instruction_files: null,
      args_json: '[]',
      autostart: 0,
      inject_hooks: 1,
      auto_approve: 1,
      identity: null,
      env_json: '{}',
      is_primary: 1,
      managed_by: 'user',
      cwd: null,
      created_at: now(),
    })
    // 先鋪歷史再鋪示範訊息：示範的那兩則要留在最新一頁，不然翻頁才看得到就失去意義。
    this.seedLongHistory(p.id)
    this.seedGroupRelay(p.id)
    installDevHelpers(this)
  }

  /** 使用者發 vs AGM 代發並排：P4 驗收缺陷 3 是兩者畫得一樣。 */
  private seedGroupRelay(projectId: string) {
    const target = this.bots.find((b) => b.project_id === projectId)
    const agm = this.bots.find((b) => b.project_id === projectId && b.id !== target?.id)
    if (!target) return
    const base = {
      conversation_id: this.conv(target.id),
      turn_id: null,
      bot_id: target.id,
      role: 'user' as const,
      source: 'web' as const,
      incomplete: 0,
    }
    this.addMessage({ ...base, content: '先看一下 README 的安裝步驟還對不對', group_id: ulid('grp') })
    this.addMessage({
      ...base,
      content: '任務 01M2C：先把設定頁的錯字修掉，做完回報，我會派 reviewer。',
      group_id: ulid('grp'),
      relay_from: agm?.id ?? 'daemon',
    })
  }

  /**
   * 第一顆 bot 的對話塞超過一頁（`PAGE_SIZE` 200）：不然 `has_more` 永遠是 false，
   * 「載入更早的訊息」在 mock 下根本長不出來，往前翻頁那條路也就從來沒被走過。
   */
  private seedLongHistory(projectId: string) {
    const target = this.bots.find((b) => b.project_id === projectId)
    if (!target) return
    const base = {
      conversation_id: this.conv(target.id),
      bot_id: target.id,
      source: 'web' as const,
      incomplete: 0,
    }
    for (let i = 1; i <= 120; i++) {
      const turnId = ulid('turn')
      this.addMessage({ ...base, turn_id: turnId, role: 'user', content: `第 ${i} 輪：把這段的測試補上` })
      this.addMessage({ ...base, turn_id: turnId, role: 'assistant', content: `第 ${i} 輪：補好了，兩條都綠。` })
    }
  }

  // transport

  session(): Promise<string> {
    return Promise.resolve('mock-ui-token')
  }

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
    const out = /^\/bots\/([^/]+)\/outbox\/file\?path=(.+)$/.exec(path)
    if (out) {
      const name = decodeURIComponent(out[2])
      return URL.createObjectURL(new Blob([`（mock）${name} 的內容\n`], { type: 'text/plain' }))
    }
    // 對話裡的本機圖片：畫一張標出路徑的示意圖，`missing` 開頭的演 404。
    const local = path.match(/^\/bots\/[^/]+\/local-image\?path=(.*)$/)
    if (local) {
      const p = decodeURIComponent(local[1])
      if (p.includes('missing')) throw new ApiError(404, { error: 'not_found', what: 'image' }, 'not found')
      const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="320" height="120"><rect width="320" height="120" fill="#dbeafe"/><text x="12" y="64" font-size="14" fill="#1e3a8a">${p.replace(/[<&]/g, '')}</text></svg>`
      return URL.createObjectURL(new Blob([svg], { type: 'image/svg+xml' }))
    }
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
    // codex usage creeps up (WS `quota_updated`).
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
    const fault = this.faults.findIndex((f) => f.method === method && f.pattern.test(path))
    if (fault >= 0) {
      const [f] = this.faults.splice(fault, 1)
      throw new ApiError(f.status, f.body, `mock fault ${f.status}`)
    }
    const [rawPath, query] = path.split('?')
    const q = new URLSearchParams(query ?? '')
    const b = (body ?? {}) as Rec
    const seg = rawPath.split('/').filter(Boolean)

    if (method === 'GET' && rawPath === '/state') return this.state()
    // 前端已樂觀套用排序，mock 收下就好。
    // 跨裝置已讀：mock 只有一個瀏覽器，記下來就好。
    { const m = rawPath.match(/^\/bots\/([^/]+)\/read$/); if (method === 'POST' && m) return { bot_id: decodeURIComponent(m[1]), read_mark: { at: typeof b.at === 'string' ? b.at : new Date().toISOString(), id: typeof b.message_id === 'string' ? b.message_id : '' }, unread: 0 } }
    // §6.5e：專案底下的非 agent pane（只回這個專案自己的，沒歸屬的走 `?unowned=1`）。
    { const m = rawPath.match(/^\/projects\/([^/]+)\/panes$/); if (method === 'GET' && m) return this.projectPanes(decodeURIComponent(m[1])) }
    // 全部被 trace 的 pane；`unowned=1` 只回對不到專案的，並標哪一顆是 scratch（§6.5e）。
    if (method === 'GET' && rawPath === '/panes') return { panes: q.get('unowned') === '1' ? this.unownedPanes() : this.allPanes() }
    { const m = rawPath.match(/^\/panes\/([^/]+)\/(focus|close|adopt)$/); if (method === 'POST' && m) {
        const pane = decodeURIComponent(m[1])
        if (m[2] === 'close') {
          const row = this.panes.find((p) => p.pane_id === pane)
          if (row?.kind === 'service' && q.get('confirm') !== 'true') {
            throw new ApiError(409, { error: 'conflict', reason: 'service_pane', pane: row }, 'service pane needs confirm')
          }
          this.panes = this.panes.filter((p) => p.pane_id !== pane)
          this.tracedScreens.delete(`${String(row?.host ?? 'local')}/${pane}`)
          return { closed: true }
        }
        return { ok: true, pane_id: pane }
      } }
    if (method === 'POST' && rawPath === '/order') return this.saveOrder(b)
    if (method === 'GET' && rawPath === '/fs/dirs') {
      return this.dirs(q.get('path') ?? '', q.get('host') ?? '', q.get('hidden') === '1')
    }
    if (method === 'GET' && rawPath === '/models') return this.models(q.get('kind') ?? '', q.get('host') ?? '', q.get('identity') ?? '')
    if (method === 'GET' && rawPath === '/quota') return { kinds: this.quota }
    if (method === 'GET' && rawPath === '/mem') return this.mem()
    if (method === 'GET' && rawPath === '/mem/processes') return this.memProcesses(q.get('host') ?? 'local')
    if (method === 'POST' && rawPath === '/mem/processes/kill') return this.killMemProcess(b)
    if (method === 'GET' && rawPath === '/mem/processes/pane') return this.memPane(q.get('host') ?? 'local', q.get('pane_id') ?? '')
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'tools' && seg[3] === 'install') return this.installTool(seg[1], b)
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'tools' && seg[3] === 'refresh') return this.refreshTools(seg[1])
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'identities' && seg[4] === 'login') return this.loginIdentity(seg[1], decodeURIComponent(seg[3]))
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'identities' && seg[4] === 'logout') return this.logoutIdentity(seg[1], decodeURIComponent(seg[3]))
    if (method === 'GET' && seg[0] === 'bots' && seg[2] === 'outbox' && seg.length === 3) {
      return this.outbox(decodeURIComponent(seg[1]))
    }
    if (method === 'GET' && seg[0] === 'identity-prefs') return { disabled: this.disabledIdentities }
    if (method === 'PUT' && seg[0] === 'identities' && seg[2] === 'disabled') {
      return this.setIdentityDisabled(decodeURIComponent(seg[1]), String(b.kind ?? ''), Boolean(b.disabled), String(b.host ?? 'local'))
    }
    if (seg[0] === 'hosts' && seg[2] === 'gh' && method === 'GET' && seg.length === 3) return this.ghStatus(seg[1])
    if (seg[0] === 'hosts' && seg[2] === 'gh' && seg[3] === 'login' && method === 'POST') return this.ghLogin(seg[1], b)
    if (seg[0] === 'hosts' && seg[2] === 'gh' && seg[3] === 'cancel' && method === 'POST') return this.ghCancel(seg[1])
    if (seg[0] === 'hosts' && seg[2] === 'shells') {
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
    if (method === 'PATCH' && seg[0] === 'projects' && seg.length === 2) return this.patchProject(seg[1], b)
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'bots') return this.addBot(seg[1], b)
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'messages') return this.projectMessages(seg[1], q)
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'submodules') return { project_id: seg[1], submodules: [] }
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'issues') return this.issues(seg[1], seg[3], q)
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'chat') return this.projectChat(seg[1], b)

    // 群組任務
    if (seg[0] === 'projects' && seg[2] === 'missions' && !this.missionsDisabled) {
      if (method === 'POST') return this.createMission(seg[1], b)
      if (method === 'GET') return this.listMissions(seg[1], q.get('status') ?? 'all', Number(q.get('limit') ?? 100))
    }
    if (seg[0] === 'missions' && seg.length >= 2 && !this.missionsDisabled) {
      const id = seg[1]
      if (method === 'GET' && seg.length === 2) return this.missionDetail(id)
      if (method === 'POST' && seg[2] === 'events') return this.addMissionEvent(id, b)
      if (method === 'POST' && (seg[2] === 'pause' || seg[2] === 'resume' || seg[2] === 'cancel')) {
        return this.controlMission(id, seg[2], b)
      }
      if (method === 'POST' && seg[2] === 'question') return this.askMission(id, b)
      if (method === 'POST' && seg[2] === 'answer') return this.answerMission(id, b)
      if (method === 'POST' && seg[2] === 'revise') return this.reviseMission(id, b)
    }

    // SPEC §6.9：須排在 `bots/{id}` 前，否則 `restart-idle` 被當成 bot id。
    if (method === 'POST' && seg[0] === 'bots' && seg[1] === 'restart-idle' && seg.length === 2) {
      return this.restartIdle()
    }

    if (seg[0] === 'bots' && seg.length >= 2) {
      const botId = seg[1]
      const action = seg[2]
      if (method === 'PATCH' && !action) return this.patchBot(botId, b)
      if (method === 'DELETE' && !action) return this.deleteBot(botId)
      if (method === 'GET' && action === 'messages') return this.messagesOf(botId, q)
      if (action === 'preview' && seg.length === 3) {
        if (method === 'GET') return this.previewOf(botId)
        if (method === 'POST') return this.startPreview(botId, b)
        if (method === 'DELETE') return this.stopPreview(botId)
      }
      if (method === 'GET' && action === 'terminal') {
        return this.terminal(botId, q.get('source') ?? 'visible', Number(q.get('lines') ?? 40))
      }
      if (method === 'POST') {
        if (action === 'start') {
          const r = this.start(botId)
          this.flushWaiting(botId)
          return r
        }
        if (action === 'stop') return this.stop(botId)
        if (action === 'restart') return this.restart(botId)
        if (action === 'fork') return this.fork(botId, b)
        if (action === 'promote') return this.promote(botId, b)
        if (action === 'interrupt') return this.interrupt(botId)
    if (action === 'abort') return this.abort(botId)
        if (action === 'login') return this.login(botId)
        if (action === 'prompt') return this.prompt(botId, b)
        if (action === 'keys') return this.keys(botId, b)
        if (action === 'text') return this.text(botId, b)
        if (action === 'pane' && seg[3] === 'move-to-tab') return this.movePaneToTab(botId)
      }
    }

    if (method === 'POST' && seg[0] === 'turns' && seg[2] === 'abandon') return this.abandon(seg[1])
    if (method === 'POST' && seg[0] === 'turns' && seg[2] === 'withdraw') return this.withdraw(seg[1])

    throw new ApiError(404, { reason: `mock: no route for ${method} ${rawPath}` }, 'not found')
  }

  // helpers

  /** `?q=gh-error` simulates a 502. */
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

  /** Unknown kind → 400; down host → 502. */
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

  /** Instructions go to a running bot on that host as an ordinary prompt. */
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

  /** SPEC §15.2：固定五列演遍四種 owner，popover 要證明的是「哪些能砍」。 */
  private memPane(host: string, paneId: string) {
    const lines = [
      `$ claude --dangerously-skip-permissions`,
      '',
      '> 幫我看一下 web/src/store/store.ts 的 refreshState 為什麼會清掉草稿',
      '',
      '⏺ 我看了 store.ts，refreshState 會整批換掉 bots slice……',
      '',
      '╭──────────────────────────────────────────────╮',
      '│ >                                            │',
      '╰──────────────────────────────────────────────╯',
      `  ${host} · ${paneId}`,
    ]
    return { host, pane_id: paneId, source: 'visible', text: lines.join('\n'), revision: 3, truncated: false, columns: 120, rows: 40 }
  }

  private memProcesses(host: string) {
    const live = this.bots.filter((x) => this.activeRun(x.id)).slice(0, 2)
    const mb = (n: number) => n * 1024 * 1024
    const rows = [
      ...live.map((b, i) => ({
        pid: 59400 + i,
        ppid: 37845,
        rss_bytes: mb(390 - i * 40),
        exe: b.kind,
        argv: `${b.kind} --dangerously-skip-permissions`,
        pane_id: `w168:p${i + 1}`,
        socket_path: '/Users/me/.config/herdr/sessions/agents-manager/herdr.sock',
        bot_id: b.id,
        bot_name: b.name,
        project_id: b.project_id,
        owner: 'bot',
        subtree_bytes: mb(398 - i * 40),
        children: 2,
      })),
      {
        pid: 51987,
        ppid: 37845,
        rss_bytes: mb(234),
        exe: 'claude',
        argv: 'claude --dangerously-skip-permissions',
        pane_id: 'wM:pB',
        socket_path: '/Users/me/.config/herdr/herdr.sock',
        bot_id: null,
        bot_name: null,
        project_id: null,
        owner: 'pane',
        subtree_bytes: mb(234),
        children: 0,
      },
      {
        pid: 1635,
        ppid: 37845,
        rss_bytes: mb(191),
        exe: 'claude',
        argv: 'claude --resume ed714d36-ba9e-4d0f-8e8d-d0bd206329d9',
        pane_id: 'wH:pC',
        bot_id: null,
        bot_name: null,
        project_id: null,
        owner: 'pane',
        subtree_bytes: mb(203),
        children: 3,
      },
      {
        pid: 88450,
        ppid: 88031,
        rss_bytes: mb(23),
        exe: 'node',
        argv: 'node scripts/dev-proxy.mjs',
        pane_id: null,
        bot_id: null,
        bot_name: null,
        project_id: null,
        owner: 'unknown',
        subtree_bytes: mb(23),
        children: 0,
      },
    ].sort((a, b2) => b2.subtree_bytes - a.subtree_bytes)
    return { host, sampled_at: new Date().toISOString(), processes: rows }
  }

  /** 同 daemon：不在清單 400、bot 409。 */
  private killMemProcess(b: Rec) {
    const host = typeof b.host === 'string' ? b.host : 'local'
    const pid = typeof b.pid === 'number' ? b.pid : -1
    const row = this.memProcesses(host).processes.find((p) => p.pid === pid)
    if (!row) throw new ApiError(400, { error: 'bad_request', message: `pid ${pid} 不在 ${host} 的 herdr 樹裡` }, 'bad request')
    if (row.owner === 'bot') {
      throw new ApiError(
        409,
        { error: 'conflict', reason: 'bot_process', message: '這是 AG Man 的 bot，請用停止 bot' },
        'conflict',
      )
    }
    this.emit('mem_updated', this.mem())
    return { host, pid, signal: typeof b.signal === 'string' ? b.signal : 'TERM', exe: row.exe, freed_bytes: row.subtree_bytes }
  }

  /**
   * §6.5e：第一個專案底下一顆 bot 開的 dev server（有 port，唯讀）、一顆跑著 vim 的 shell（掃描分成 service，
   * 但沒有 port，打得進去）、一顆使用者自己開的 shell；另外兩顆對不到專案：scratch 與「多出來的」。
   */
  private panes: Array<Record<string, unknown>> = []

  private seedPanes() {
    if (this.panes.length > 0) return
    const bot = this.bots[0]
    const pid = this.projects[0]?.id ?? null
    const ago = (m: number) => new Date(Date.now() - m * 60000).toISOString()
    const row = (over: Record<string, unknown>) => ({
      host: 'local', workspace_id: 'w168', tab_id: 'w168:t39', cwd: '/Users/me/project/agents-manager',
      kind: 'shell', owned_by: 'user', owner_bot_id: null, project_id: pid, purpose: null, foreground: null,
      listen_ports: [], last_output_at: ago(5), first_seen: ago(600), last_seen: ago(0), gc_optin: false, ...over,
    })
    this.panes = [
      row({ pane_id: 'w168:p62', kind: 'service', owned_by: 'bot', owner_bot_id: bot?.id ?? null, purpose: 'dev-server',
        foreground: 'node next dev --port 3010', listen_ports: [3010], last_output_at: ago(1), first_seen: ago(180) }),
      row({ pane_id: 'w168:p63', kind: 'service', foreground: 'vim notes.md', last_output_at: ago(2), first_seen: ago(90) }),
      row({ pane_id: 'w1HJ:p4W', workspace_id: 'w1HJ', tab_id: 'w1HJ:t2K', cwd: '/Users/me/project/agents-manager/web',
        last_output_at: ago(420) }),
      row({ pane_id: 'w9:p1', workspace_id: 'w9', tab_id: 'w9:t1', cwd: '/Users/me', owned_by: 'none', project_id: null,
        first_seen: ago(3000) }),
      row({ pane_id: 'w9:p7', workspace_id: 'w9', tab_id: 'w9:t7', cwd: '/tmp', owned_by: 'none', project_id: null,
        last_output_at: ago(400), first_seen: ago(500) }),
    ]
  }

  /** daemon 的唯讀規則：有 listen port 才唯讀（`shell::allowed`），跟 kind 無關。 */
  private paneRow(p: Record<string, unknown>): Record<string, unknown> {
    return { ...p, read_only: Array.isArray(p.listen_ports) && p.listen_ports.length > 0 }
  }

  private allPanes() {
    this.seedPanes()
    return this.panes.map((p) => this.paneRow(p))
  }

  private projectPanes(projectId: string) {
    this.seedPanes()
    const project = this.projects.find((p) => p.id === projectId)
    return { project_id: projectId, host: project?.host ?? 'local', panes: this.allPanes().filter((p) => p.project_id === projectId) }
  }

  /** scratch＝對不到專案的 shell 裡 `first_seen` 最早的那顆，由 daemon 標，前端不重算。 */
  private unownedPanes() {
    const rows = this.allPanes().filter((p) => p.project_id === null)
    const scratch = rows.filter((p) => p.kind === 'shell').sort((a, b) => String(a.first_seen).localeCompare(String(b.first_seen)))[0]
    return rows.map((p) => ({ ...p, scratch: p === scratch }))
  }

  /** SPEC §15：依執行中 bot 數推算，啟停 bot 時數字才會動。 */
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
        // 超線的瀏覽器分頁，演警示。
        browsers:
          h.name === 'local'
            ? [
                { name: 'Chrome', tabs: 34, bytes: 3.2 * 1024 ** 3, processes: 41 },
                { name: 'ego', tabs: 6, bytes: 700 * 1024 ** 2, processes: 9 },
              ]
            : [],
        // 整機記憶體（2026-09-12）：遠端給吃緊的，演 `.mem-low`。
        machine:
          h.name === 'local'
            ? { total_bytes: 16 * 1024 ** 3, available_bytes: 5.5 * 1024 ** 3 }
            : { total_bytes: 32 * 1024 ** 3, available_bytes: 3 * 1024 ** 3 },
      }
    })
    return {
      total_bytes: rows.reduce((n, r) => n + r.total_bytes, 0),
      herdr_bytes: rows.reduce((n, r) => n + r.herdr_bytes, 0),
      agents_bytes: rows.reduce((n, r) => n + r.agents_bytes, 0),
      processes: rows.reduce((n, r) => n + r.processes, 0),
      hosts: rows,
      projects: this.projects
        .map((p) => {
          const live = this.bots.filter((b) => b.project_id === p.id && this.activeRun(b.id))
          return { project_id: p.id, host: p.host, panes: live.length, bytes: live.reduce((n, b) => n + (PER_BOT[b.kind] ?? 500) * 1024 * 1024, 0) }
        })
        .filter((r) => r.panes > 0),
    }
  }

  /** SPEC §11.5. */
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
          // Long level to exercise picker scrolling/filtering.
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

  // identities

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
    // 新身份尚未偵測：mock 直接給「未知」。
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

  /** 答案與上次相同；重點是端點形狀與 busy 狀態。 */
  private refreshTools(host: string) {
    const remote = host && host !== 'local' ? this.host(host) : null
    const tools = remote ? remote.tools : this.localTools
    const identities = remote ? remote.identities : this.localIdentityStatus
    return { name: remote?.name ?? 'local', tools, identities, tools_checked_at: now() }
  }

  private loginIdentity(host: string, name: string) {
    const remote = host && host !== 'local' ? this.host(host) : null
    const identities = remote ? remote.identities : this.localIdentityStatus
    const st = identities[name]
    if (!st) throw new ApiError(404, { error: 'not_found', what: 'identity' }, 'identity not found')
    const shell = this.openShell(host, '')
    const pane = this.shell(host, shell.pane_id)
    const dir = st.config_dir ? `CLAUDE_CONFIG_DIR='${st.config_dir}' ` : ''
    const command = st.kind === 'claude' ? `${dir}claude /login` : `${dir}${st.kind} login`
    pane.lines.push(`${pane.cwd.split('/').pop() ?? '~'} % ${command}`, '請在瀏覽器完成登入：', 'https://example.test/device?code=AM-MOCK', '登入完成，正在重新偵測…', '')
    st.logged_in = true
    st.account = st.account ?? 'mock@example.com'
    this.emit('host_changed', { name: host || 'local', connected: true, identities })
    return shell
  }

  /** 登出：跟登入同一條路，只是指令與結果相反（帳號憑證被清掉）。 */
  private logoutIdentity(host: string, name: string) {
    const remote = host && host !== 'local' ? this.host(host) : null
    const identities = remote ? remote.identities : this.localIdentityStatus
    const st = identities[name]
    if (!st) throw new ApiError(404, { error: 'not_found', what: 'identity' }, 'identity not found')
    const shell = this.openShell(host, '')
    const pane = this.shell(host, shell.pane_id)
    const dir = st.config_dir ? `CLAUDE_CONFIG_DIR='${st.config_dir}' ` : ''
    const command = st.kind === 'claude' ? `${dir}claude /logout` : `${dir}${st.kind} logout`
    pane.lines.push(`${pane.cwd.split('/').pop() ?? '~'} % ${command}`, '已登出，憑證已清除。', '')
    st.logged_in = false
    st.account = undefined
    this.emit('host_changed', { name: host || 'local', connected: true, identities })
    return shell
  }

  private setIdentityDisabled(name: string, kind: string, disabled: boolean, host: string) {
    const key = `${host || 'local'}|${kind}|${name}`
    this.disabledIdentities = disabled
      ? this.disabledIdentities.includes(key)
        ? this.disabledIdentities
        : [...this.disabledIdentities, key]
      : this.disabledIdentities.filter((k) => k !== key)
    this.emit('identity_prefs_changed', { host: host || 'local', kind, identity: name, disabled })
    return { host: host || 'local', kind, identity: name, disabled }
  }

  /** 跟 daemon 同一個形狀（SPEC §6.5f）：新的排前面，每個檔案帶剩餘秒數；grok 這顆演遠端。 */
  /** bot 在 mock 回合裡「放進 outbox」的檔名（`outbox` 關鍵字），依序累積。 */
  private outboxAdded = new Map<string, string[]>()

  private outbox(botId: string) {
    const bot = this.bot(botId)
    const ttl = 3600
    if (bot.kind === 'grok') return { files: [], ttl_secs: ttl, reason: 'outbox_remote' }
    const now = Math.floor(Date.now() / 1000)
    const file = (name: string, size: number, age: number) => ({
      name,
      size,
      modified: now - age,
      expires_at: now - age + ttl,
      remaining_secs: Math.max(0, ttl - age),
    })
    return {
      dir: `/Users/me/.config/agents-manager/outbox/${botId}`,
      ttl_secs: ttl,
      files: [
        ...(this.outboxAdded.get(botId) ?? []).map((name) => file(name, 141, 5)),
        file('tracking.tsv', 18_432, 120),
        file('出貨追蹤 v2.md', 4_096, 1_500),
        file('run.log', 1_204_233, 3_300),
      ],
    }
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

  // hosts (SPEC §11.6)

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

  /** Targets with `fail` / `bad` / unreachable-looking stay down. */
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
      // grok missing (exercises the tools hint).
      tools: { claude: { ...TOOLS_ALL_OK.claude }, codex: { ...TOOLS_ALL_OK.codex }, grok: { installed: false, path: null, version: null, logged_in: null } },
      identities: {
        ...Object.fromEntries(
          this.identities.map((i) => [
            i.name,
            Object.keys(i.env).length === 0
              ? { name: i.name, kind: i.kind, logged_in: true, account: `${name}@example.com`, source: 'config' as const }
              : { name: i.name, kind: i.kind, logged_in: false, source: 'config' as const },
          ]),
        ),
        // 同名 `ccN` 指到那台自己的目錄（SPEC §16）。
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
    // API.md: existing name = update (disconnect, reconnect).
    this.hosts = this.hosts.filter((x) => x.name !== name)
    this.hosts.push(h)
    await sleep(700) // ssh master + remote `herdr session list` take a moment
    this.dial(h)
    this.seedHostQuota(h)
    this.emit('host_changed', { name: h.name, connected: h.connected, error: h.error })
    this.emitMem()
    return { name: h.name, connected: h.connected, error: h.error }
  }

  /** 連上才有；斷線主機留空。 */
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
    // 同 daemon：主機刪掉，額度列也移除（SPEC §14）。
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

  private emitMem() {
    this.emit('mem_updated', this.mem())
  }

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

  // 群組任務

  private missionStatus(m: MockMission): string {
    return m.cancelled_at ? 'cancelled' : m.completed_at ? 'done' : m.paused_reason ? 'paused' : 'open'
  }

  private missionJson(m: MockMission): Rec {
    const status = this.missionStatus(m)
    // P1b：`phase` = 最新一件未結交辦的 role。
    const mine = this.missionAssignments.filter((a) => a.mission_id === m.id)
    const openOne = [...mine].reverse().find((a) => !a.completed_at)
    const phase =
      status !== 'open'
        ? status
        : openOne
          ? openOne.status === 'quota_blocked'
            ? 'waiting_quota'
            : openOne.role === 'reviewer'
              ? 'reviewing'
              : openOne.role === 'verifier'
                ? 'verifying'
                : 'executing'
          : mine.length
            ? 'awaiting_agm'
            : 'planning'
    return { ...m, status, phase }
  }

  private missionAssignment(missionId: string, over: Partial<MockMissionAssignment>): MockMissionAssignment {
    const a: MockMissionAssignment = {
      id: ulid('asg'),
      mission_id: missionId,
      role: 'executor',
      status: 'completed',
      target_bot_id: null,
      turn_status: null,
      turn_error: null,
      follow_up_of: null,
      resume_at: null,
      created_at: now(),
      completed_at: now(),
      ...over,
    }
    this.missionAssignments.push(a)
    return a
  }

  private missionEvent(
    missionId: string,
    kind: string,
    text: string,
    payload: Rec | null = null,
    relayFrom: string | null = null,
    replyTo: string | null = null,
  ) {
    const e: MockMissionEvent = {
      id: ulid('mev'),
      mission_id: missionId,
      kind,
      text,
      relay_from: relayFrom,
      payload,
      reply_to: replyTo,
      client_request_id: null,
      created_at: now(),
    }
    this.missionEvents.push(e)
    return e
  }

  private touchMission(m: MockMission) {
    m.updated_at = now()
    this.emit('mission_updated', {
      mission_id: m.id,
      project_id: m.project_id,
      status: this.missionJson(m).status,
    })
  }

  private createMission(projectId: string, b: Rec): Rec {
    const reqId = String(b.client_request_id ?? '')
    const same = reqId ? this.missions.find((m) => m.client_request_id === reqId) : undefined
    if (same) return { mission: this.missionJson(same), created: false }
    const m: MockMission = {
      id: ulid('mis'),
      project_id: projectId,
      client_request_id: reqId,
      text: String(b.text ?? ''),
      parent_mission_id: null,
      delivery_mode: b.delivery_mode === 'push_main' ? 'push_main' : 'pr',
      executor_kind: (b.executor_kind === 'codex' || b.executor_kind === 'grok' ? b.executor_kind : 'claude') as BotKind,
      on_5h_limit: b.on_5h_limit === 'switch' ? 'switch' : 'wait',
      max_rounds: typeof b.max_rounds === 'number' ? b.max_rounds : 2,
      rounds_used: 0,
      paused_reason: null,
      paused_detail: null,
      result_summary: null,
      created_at: now(),
      updated_at: now(),
      completed_at: null,
      cancelled_at: null,
    }
    this.missions.unshift(m)
    this.missionEvent(m.id, 'instruction', m.text)
    this.touchMission(m)
    return { mission: this.missionJson(m), created: true }
  }

  private listMissions(projectId: string, status: string, limit: number): Rec {
    const all = this.missions.filter((m) => m.project_id === projectId)
    const keep = all.filter((m) => {
      const s = this.missionJson(m).status
      return status === 'all' || s === status || (status === 'open' && s === 'paused')
    })
    // 照 daemon：新的在前，`limit` 夾在 1..=500。
    const cap = Math.min(500, Math.max(1, Number.isFinite(limit) ? limit : 100))
    return { project_id: projectId, missions: keep.slice(0, cap).map((m) => this.missionJson(m)) }
  }

  private missionDetail(id: string): Rec {
    const m = this.missions.find((x) => x.id === id)
    if (!m) throw new ApiError(404, { error: 'not_found' }, 'mission not found')
    const parent = m.parent_mission_id ? this.missions.find((x) => x.id === m.parent_mission_id) : undefined
    return {
      ...this.missionJson(m),
      events: this.missionEvents.filter((e) => e.mission_id === id),
      assignments: this.missionAssignments.filter((a) => a.mission_id === id),
      revisions: this.missions
        .filter((x) => x.parent_mission_id === id)
        .map((x) => ({ id: x.id, text: x.text, status: this.missionStatus(x), created_at: x.created_at, result_summary: x.result_summary })),
      parent: m.parent_mission_id
        ? parent
          ? { id: parent.id, text: parent.text, status: this.missionStatus(parent), result_summary: parent.result_summary }
          : { id: m.parent_mission_id, missing: true }
        : null,
    }
  }

  private addMissionEvent(id: string, b: Rec): Rec {
    const m = this.missions.find((x) => x.id === id)
    if (!m) throw new ApiError(404, { error: 'not_found' }, 'mission not found')
    const e = this.missionEvent(
      id,
      String(b.kind ?? 'note'),
      String(b.text ?? ''),
      (b.payload ?? null) as Rec | null,
      b.relay_from ? String(b.relay_from) : null,
    )
    this.touchMission(m)
    return { event: e }
  }

  /** 同 daemon：只留言，狀態不動。 */
  private askMission(id: string, b: Rec): Rec {
    const m = this.missions.find((x) => x.id === id)
    if (!m) throw new ApiError(404, { error: 'not_found' }, 'mission not found')
    const dup = this.missionEvents.find((e) => e.mission_id === id && e.client_request_id === String(b.client_request_id ?? ''))
    if (dup) return { event: dup, replayed: true }
    const e = this.missionEvent(id, 'question', String(b.text ?? ''))
    e.client_request_id = String(b.client_request_id ?? '')
    this.touchMission(m)
    // demo：AGM 隔一會兒回一句。
    setTimeout(() => {
      const a = this.missionEvent(id, 'answer', '看過了：這個改動只動到文案，不影響登入流程。', null, 'bot-agm', e.id)
      a.client_request_id = `${e.client_request_id}-reply`
      this.touchMission(m)
    }, 900)
    return { event: e, replayed: false }
  }

  private answerMission(id: string, b: Rec): Rec {
    const m = this.missions.find((x) => x.id === id)
    if (!m) throw new ApiError(404, { error: 'not_found' }, 'mission not found')
    const e = this.missionEvent(id, 'answer', String(b.text ?? ''))
    e.client_request_id = String(b.client_request_id ?? '')
    const resumed = Boolean(m.paused_reason)
    if (resumed) {
      m.paused_reason = null
      m.paused_detail = null
      this.missionEvent(id, 'resumed', '繼續', null, 'daemon')
    }
    this.touchMission(m)
    return { event: e, replayed: false, resumed, mission: this.missionJson(m) }
  }

  /** 開新任務，原成果不動。 */
  private reviseMission(id: string, b: Rec): Rec {
    const parent = this.missions.find((x) => x.id === id)
    if (!parent) throw new ApiError(404, { error: 'not_found' }, 'mission not found')
    if (!parent.completed_at) throw new ApiError(409, { error: 'conflict', reason: 'not_completed' }, 'not completed')
    const open = this.missions.find((x) => x.parent_mission_id === id && !x.completed_at && !x.cancelled_at)
    if (open) throw new ApiError(409, { error: 'conflict', reason: 'revision_in_progress', mission_id: open.id }, 'revision in progress')
    const child: MockMission = {
      ...parent,
      id: ulid('msn'),
      client_request_id: String(b.client_request_id ?? ''),
      text: String(b.text ?? ''),
      parent_mission_id: parent.id,
      paused_reason: null,
      paused_detail: null,
      result_summary: null,
      rounds_used: 0,
      created_at: now(),
      updated_at: now(),
      completed_at: null,
      cancelled_at: null,
    }
    this.missions.unshift(child)
    this.missionEvent(child.id, 'instruction', child.text)
    this.missionEvent(id, 'note', `追加修改：已開續作任務 ${child.id}`, { revision_mission_id: child.id }, 'daemon')
    this.touchMission(parent)
    this.emit('mission_updated', { mission_id: child.id, project_id: child.project_id, status: 'open' })
    return { ...this.missionJson(child), created: true }
  }

  private controlMission(id: string, action: 'pause' | 'resume' | 'cancel', b: Rec): Rec {
    const m = this.missions.find((x) => x.id === id)
    if (!m) throw new ApiError(404, { error: 'not_found' }, 'mission not found')
    if (m.completed_at || m.cancelled_at) throw new ApiError(409, { error: 'already_closed' }, 'closed')
    if (action === 'pause') {
      // 照 daemon：`PauseIn.reason` 必填，沒帶是 axum 的 422。mock 以前自己補 'manual'，所以 M4 一直沒被發現。
      if (typeof b.reason !== 'string' || !b.reason.trim()) {
        throw new ApiError(422, { error: 'unprocessable', message: 'missing field `reason`' }, 'missing field `reason`')
      }
      m.paused_reason = b.reason.trim()
      m.paused_detail = b.detail ? String(b.detail) : null
      this.missionEvent(id, 'paused', m.paused_detail ?? '已暫停', null, 'daemon')
    }
    if (action === 'resume') {
      m.paused_reason = null
      m.paused_detail = null
      this.missionEvent(id, 'resumed', '繼續', null, 'daemon')
    }
    if (action === 'cancel') {
      m.cancelled_at = now()
      this.missionEvent(id, 'cancelled', '已取消', null, 'daemon')
    }
    this.touchMission(m)
    return { mission: this.missionJson(m) }
  }

  /**
   * 跑到一半／停下來問人／已完成各一。payload 一律照 daemon 真的寫的形狀：AGM 用 `agm mission event` 回報
   * 不帶 payload（CLI 沒有這個參數），角色與 bot 看交辦；換手 note、驗證者沒 Fable 的暫停、`verified` 的 sha
   * 都是 daemon 寫的（出處見 `lib/missionView.ts`）。以前這裡自編的 `handoff/from/to`、頂層 `resets` 真 daemon 從來不寫（review3 c1 L9）。
   */
  private seedMissions(projectId: string) {
    const mk = (over: Partial<MockMission>): MockMission => ({
      id: ulid('mis'),
      project_id: projectId,
      client_request_id: ulid('req'),
      text: '',
      parent_mission_id: null,
      delivery_mode: 'pr',
      executor_kind: 'claude',
      on_5h_limit: 'wait',
      max_rounds: 2,
      rounds_used: 0,
      paused_reason: null,
      paused_detail: null,
      result_summary: null,
      created_at: now(),
      updated_at: now(),
      completed_at: null,
      cancelled_at: null,
      ...over,
    })

    const running = mk({ text: '把設定頁的錯字修掉，順便補一個 tsc 的 CI 檢查', delivery_mode: 'push_main', rounds_used: 1 })
    this.missions.push(running)
    this.missionEvent(running.id, 'instruction', running.text)
    this.missionEvent(running.id, 'report', '拆成兩塊：錯字（4 處）與 CI workflow', null, 'bot-agm')
    const first = this.missionAssignment(running.id, {
      role: 'executor',
      target_bot_id: 'mission-exec',
      status: 'failed',
      turn_status: 'identity_switch',
      turn_error: "You've hit your usage limit",
    })
    this.missionEvent(running.id, 'note', 'cc2 撞到用量上限，換 cc1 接手', {
      mission_id: running.id,
      assignment_id: first.id,
      role: 'executor',
      bot_id: 'mission-exec',
      from_identity: 'cc2',
      to_identity: 'cc1',
      model: null,
      reason: 'cc2 的週額度用完，換下一個身分',
      message: "You've hit your usage limit",
      needs_review: true,
    }, 'daemon')
    this.missionEvent(running.id, 'report', '錯字改好了，CI workflow 還在寫', null, 'bot-agm')
    this.missionEvent(running.id, 'report', 'changes：workflow 少了 bun install 的快取', null, 'bot-agm')
    this.missionEvent(running.id, 'round', '第 1 輪退回（上限 2）', { rounds_used: 1 }, 'daemon')
    this.missionAssignment(running.id, { role: 'executor', target_bot_id: 'mission-exec-2', follow_up_of: first.id })
    this.missionAssignment(running.id, {
      role: 'reviewer',
      target_bot_id: 'mission-rev',
      status: 'delivered',
      completed_at: null,
    })

    const asking = mk({
      text: '把 blocked 選單那幾個元件的測試補起來',
      paused_reason: 'no_fable_for_verifier',
      paused_detail: '三個身分的 Fable 週桶都見底了',
    })
    this.missions.push(asking)
    this.missionEvent(asking.id, 'instruction', asking.text)
    this.missionEvent(asking.id, 'report', '補了 12 個測試，tsc 與 lint 都過', null, 'bot-agm')
    this.missionEvent(
      asking.id,
      'paused',
      '暫停：三個身分的 Fable 週桶都見底了，等使用者決定',
      {
        reason: 'no_fable_for_verifier',
        decision: {
          decision: 'ask_user',
          reason: '三個身分的 Fable 週桶都見底了',
          resets: [{ identity: 'cc2', resets_at: inHours(9) }, { identity: 'cc1', resets_at: inHours(31) }],
        },
      },
      'daemon',
    )
    this.missionAssignment(asking.id, { role: 'executor', target_bot_id: 'mission-exec-3' })

    // P1b `awaiting_agm`：交辦都結案、任務還開著。
    const idle = mk({ text: '把群組未讀數改成只算 bot 的回覆', delivery_mode: 'push_main' })
    this.missions.push(idle)
    this.missionEvent(idle.id, 'instruction', idle.text)
    this.missionEvent(idle.id, 'report', '第一版做完，等 AGM 派 reviewer', null, 'bot-agm')
    this.missionAssignment(idle.id, { role: 'executor', target_bot_id: 'mission-exec-5' })
    this.missionEvent(
      idle.id,
      'note',
      '找不到跟執行者不同的身分當 reviewer：改走執行者自審＋驗證者把關',
      { decision: { decision: 'no_independent_reviewer', reason: '只有 cc2 還有額度' } },
      'daemon',
    )

    // 等額度：交辦停在 quota_blocked。
    const quota = mk({ text: '把 hosts 面板的錯誤訊息翻成中文', on_5h_limit: 'wait' })
    this.missions.push(quota)
    this.missionEvent(quota.id, 'instruction', quota.text)
    this.missionAssignment(quota.id, {
      role: 'executor',
      target_bot_id: 'mission-exec-6',
      status: 'quota_blocked',
      resume_at: inHours(2),
      completed_at: null,
    })

    const done = mk({
      text: '群組訊息的時間戳改成本地時區',
      completed_at: now(),
      result_summary: '改了 3 個檔案，時間戳統一走 Intl；驗證者跑過 tsc / oxlint / build 與 390px 截圖。',
    })
    this.missions.push(done)
    this.missionEvent(done.id, 'instruction', done.text)
    this.missionEvent(done.id, 'report', '改好了', null, 'bot-agm')
    this.missionEvent(done.id, 'report', 'approve', null, 'bot-agm')
    this.missionEvent(done.id, 'verified', 'tsc 0 錯、oxlint 0 新警告、build 過、390px 截圖 2 張', { sha: '3f2a9c1d0b7e4a5f6c8d9e0a1b2c3d4e5f6a7b8c' }, 'bot-agm')
    this.missionEvent(done.id, 'delivered', '已開 PR', { mode: 'pr', branch: 'mission/demo', url: 'https://github.com/edansun/agents-manager/pull/42' }, 'daemon')
    this.missionEvent(done.id, 'completed', done.result_summary ?? '', null, 'daemon')
    this.missionAssignment(done.id, { role: 'executor', target_bot_id: 'mission-exec-4' })
    this.missionAssignment(done.id, { role: 'reviewer', target_bot_id: 'mission-rev-2' })
    this.missionAssignment(done.id, { role: 'verifier', target_bot_id: 'mission-ver' })

    const killed = mk({ text: '把側欄改成可以拖曳排序', cancelled_at: now() })
    this.missions.push(killed)
    this.missionEvent(killed.id, 'instruction', killed.text)
    this.missionEvent(killed.id, 'cancelled', '使用者取消', null, 'daemon')
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

  private paneColumns(run: MockRun): number {
    if (run.own_tab) return WORKSPACE_COLUMNS
    const shared =
      this.foreignPanes +
      this.runs.filter((r) => !r.own_tab && r.workspace_id === run.workspace_id && this.activeRun(r.bot_id) === r).length
    return Math.max(8, Math.floor(WORKSPACE_COLUMNS / Math.max(1, shared)))
  }

  /** 搬既有 pane：`pane_id` 不變，run 不重開。 */
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
    // API.md §8：`connected` 是 bot 所屬 host 的狀態，遠端要帶 `host`；送本機值會把斷線主機標回已連線。
    const bot = this.bots.find((x) => x.id === botId)
    const project = bot ? this.projects.find((p) => p.id === bot.project_id) : undefined
    const hostName = project?.host && project.host !== 'local' ? project.host : null
    const host = hostName ? this.hosts.find((h) => h.name === hostName) : undefined
    this.emit('bot_status', {
      bot_id: botId,
      run: this.activeRun(botId) ?? null,
      connected: host ? host.connected : this.connected,
      ...(hostName ? { host: hostName } : {}),
    })
    // Only run start/stop moves the number, so ride that instead of a `setInterval`.
    this.emit('mem_updated', this.mem())
  }

  private addMessage(
    m: Omit<MockMessage, 'id' | 'created_at' | 'group_id' | 'attachments_json' | 'relay_from'> & {
      group_id?: string | null
      attachments_json?: string | null
      relay_from?: string | null
    },
  ): MockMessage {
    const msg: MockMessage = {
      group_id: null,
      attachments_json: null,
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

  // routes

  /** Mirrors `daemon/src/api.rs::state_json`. */
  private state() {
    return {
      daemon_seq: this.seq,
      connected: this.connected,
      default_connected: false,
      herdr_session: 'agents-manager',
      identities: this.identities.map((i) => ({ ...i, env: { ...i.env }, args: [...i.args] })),
      // API.md: reserved `local` entry first, null ssh fields.
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
          herdr: { server_version: '0.9.1', protocol: 22, protocol_supported: true, cli_version: '0.9.1', mismatch: false },
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
          herdr: h.connected
            ? { server_version: '0.8.2', protocol: 20, protocol_supported: true, cli_version: '0.9.1', mismatch: true }
            : { server_version: null, protocol: null, protocol_supported: null, cli_version: null, mismatch: false },
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
              instruction_files: b.kind === 'claude' ? (b.instruction_files ?? 'claude-md') : null,
              args: JSON.parse(b.args_json) as string[],
              autostart: b.autostart === 1,
              inject_hooks: b.inject_hooks === 1,
              auto_approve: b.auto_approve === 1,
              identity: b.identity,
              env: JSON.parse(b.env_json) as Record<string, string>,
              managed_by: b.managed_by,
              primary: b.is_primary === 1,
              primary_position: b.primary_position ?? 0,
              cwd: b.cwd,
              // #353：mock 也從目前 run 的啟動值投影 needs_restart，讓設定面板與真 daemon 同步。
              needs_restart:
                run !== null &&
                (run.runtime_model !== b.model ||
                  run.runtime_effort !== b.effort ||
                  run.runtime_fast !== (b.fast === 1) ||
                  (run.runtime_identity !== null && (run.runtime_identity || null) !== b.identity)),
              // 同 daemon：`slug(label)-<bot id 末 6 碼>`。
              agent_name: `${p.label.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '') || 'b'}-${b.id.slice(-6).toLowerCase()}`,
              run,
              in_flight_turn: this.turns.find((t) => run && t.run_id === run.id && t.status === 'in_flight') ?? null,
              unread: 0,
            }
          }),
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
      github: /-/.test(canonical.split('/').pop() ?? '') ? { owner: 'me', repo: canonical.split('/').pop() ?? 'repo', url: `https://github.com/me/${canonical.split('/').pop() ?? 'repo'}` } : null,
      created_at: now(),
    }
    this.projects.push(p)
    this.emit('project_changed', { project_id: p.id })
    return { project_id: p.id }
  }

  private patchProject(id: string, b: Rec) {
    const p = this.projects.find((x) => x.id === id)
    if (!p) throw new ApiError(404, { reason: 'project' }, 'not_found')
    if (b.label !== undefined) {
      const label = String(b.label ?? '').trim()
      if (!label) throw new ApiError(400, { reason: 'project label must not be empty' }, 'bad_request')
      p.label = label
    }
    this.emit('project_changed', { project_id: id })
    return { project_id: id, needs_restart: false }
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
    if (!isValidBotName(name)) {
      throw new ApiError(400, { reason: `name：${BOT_NAME_HINT}` }, 'bad request')
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
      instruction_files: toKind(b.kind) === 'claude' && typeof b.instruction_files === 'string' && b.instruction_files.trim() ? b.instruction_files.trim() : null,
      args_json: JSON.stringify(Array.isArray(b.args) ? b.args : []),
      autostart: b.autostart ? 1 : 0,
      inject_hooks: 1,
      auto_approve: b.auto_approve === false ? 0 : 1,
      identity: typeof b.identity === 'string' && b.identity.trim() ? b.identity : null,
      env_json: JSON.stringify(b.env && typeof b.env === 'object' ? b.env : {}),
      managed_by: 'user',
      cwd: typeof b.cwd === 'string' && b.cwd.trim() ? b.cwd.trim() : null,
      created_at: now(),
    }
    if (bot.identity) this.checkIdentity(bot.identity, bot.kind)
    this.bots.push(bot)
    this.emit('bot_changed', { bot_id: bot.id })
    return { bot_id: bot.id, name: bot.name }
  }

  /** `POST /order` 的 bots 部分：照給的 id 順序重排該專案的 bot（沒列到的留在後面）。 */
  private saveOrder(b: Rec) {
    const bots = b.bots && typeof b.bots === 'object' ? (b.bots as Record<string, unknown>) : {}
    for (const [pid, ids] of Object.entries(bots)) {
      if (!Array.isArray(ids)) continue
      const rank = new Map(ids.map((id, i) => [String(id), i]))
      const mine = this.bots.filter((x) => x.project_id === pid)
      const sorted = [...mine].sort((x, y) => (rank.get(x.id) ?? Infinity) - (rank.get(y.id) ?? Infinity))
      let k = 0
      this.bots = this.bots.map((x) => (x.project_id === pid ? sorted[k++] : x))
    }
    // #344：主力那列的順序，陣列位置寫進 primary_position；未知 bot id 回 400，不在陣列裡的維持原值。
    if (Array.isArray(b.primary)) {
      const ids = b.primary.map(String)
      const unknown = ids.find((id) => !this.bots.some((x) => x.id === id))
      if (unknown) throw new ApiError(400, { error: 'bad_request', message: `unknown bot ${unknown}` }, 'bad request')
      ids.forEach((id, i) => {
        this.bots.find((x) => x.id === id)!.primary_position = i
      })
    }
    return { ok: true }
  }

  /** 規則照 `daemon/src/promote.rs`：只收 child；同一顆升成頂層（保留對話），名字預設 `<名>-1`。 */
  private promote(botId: string, b: Rec) {
    const bot = this.bots.find((x) => x.id === botId)
    if (!bot) throw new ApiError(404, { error: 'not_found', what: 'bot' }, 'not found')
    if (bot.managed_by !== 'child') throw new ApiError(409, { error: 'conflict', reason: 'not_child' }, 'not child')
    const wanted = typeof b.name === 'string' && b.name.trim() ? b.name.trim() : bot.name
    let name = wanted
    for (let n = 1; this.bots.some((x) => x.id !== bot.id && x.name === name); n += 1) name = `${wanted}-${n}`
    bot.name = name
    bot.managed_by = 'user'
    delete bot.parent_bot_id
    return { bot_id: bot.id, name, promoted_from: { bot_id: bot.id, session_id: 'mock-session' }, run_id: null }
  }

  /** 規則照 `daemon/src/fork.rs`：只給頂層 bot；設定照抄、autostart 關、名字 `<來源>-fork` 撞名加尾碼。 */
  private fork(botId: string, b: Rec) {
    const src = this.bots.find((x) => x.id === botId)
    if (!src) throw new ApiError(404, { error: 'not_found', what: 'bot' }, 'not found')
    // mock 沒有 child bot（`managed_by` 恆為 user），所以不必擋 `fork_child`。
    const wanted = typeof b.name === 'string' && b.name.trim() ? b.name.trim() : `${src.name.slice(0, 27)}-fork`
    let name = wanted
    for (let n = 1; this.bots.some((x) => x.name === name); n += 1) name = `${wanted}-${n}`
    const created = this.addBot(src.project_id, {
      name,
      kind: src.kind,
      model: src.model,
      effort: src.effort,
      fast: src.fast === 1,
      persona: src.persona,
      instruction_files: src.instruction_files,
      args: JSON.parse(src.args_json) as unknown,
      identity: src.identity,
      env: JSON.parse(src.env_json) as unknown,
      auto_approve: src.auto_approve === 1,
      cwd: src.cwd,
    })
    this.addMessage({
      conversation_id: this.conv(created.bot_id),
      turn_id: null,
      bot_id: created.bot_id,
      role: 'system',
      content: `從 ${src.name} fork 出來：接續它到目前為止的完整對話脈絡（mock），之後各走各的。分叉前的訊息請到 ${src.name} 看。`,
      source: 'system',
      incomplete: 0,
    })
    // 跟 daemon 一樣排在來源正下方。
    const made = this.bots.findIndex((x) => x.id === created.bot_id)
    const [bot] = this.bots.splice(made, 1)
    this.bots.splice(this.bots.findIndex((x) => x.id === src.id) + 1, 0, bot)
    const { run_id } = this.start(created.bot_id)
    return { ...created, forked_from: { bot_id: src.id, session_id: 'mock-session' }, run_id, start_error: null }
  }

  /** 須存在且 kind 相符（API.md identities）。 */
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

  /** API.md v3.3：有 active Run 時改名 → 409；其他欄位回 `needs_restart`。 */
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
      if (!isValidBotName(name)) {
        throw new ApiError(400, { error: 'bad_request', message: `name：${BOT_NAME_HINT}` }, 'bad request')
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
    if (b.instruction_files !== undefined) bot.instruction_files = typeof b.instruction_files === 'string' && b.instruction_files.trim() ? b.instruction_files.trim() : null
    if (b.autostart !== undefined) bot.autostart = b.autostart ? 1 : 0
    if (b.auto_approve !== undefined) bot.auto_approve = b.auto_approve ? 1 : 0
    if (b.inject_hooks !== undefined) bot.inject_hooks = b.inject_hooks ? 1 : 0
    if (b.primary !== undefined) bot.is_primary = b.primary ? 1 : 0
    this.emit('bot_changed', { bot_id: id })
    // API.md §10.2: 只有影響啟動 argv / env 的欄位才需要重啟。
    const LAUNCH_FIELDS = ['model', 'effort', 'fast', 'persona', 'instruction_files', 'args', 'identity', 'env', 'auto_approve', 'inject_hooks']
    let needs_restart = run !== undefined && LAUNCH_FIELDS.some((k) => b[k] !== undefined)
    // 同 daemon `apply_live_setting`：slash 指令當場套用；清成 CLI 預設沒有對應指令。
    const only = (...fields: string[]) =>
      fields.every((f) => b[f] !== undefined) && LAUNCH_FIELDS.filter((k) => !fields.includes(k)).every((k) => b[k] === undefined)
    /** 改的欄位全在 `fields` 內。 */
    const within = (...fields: string[]) => LAUNCH_FIELDS.filter((k) => !fields.includes(k)).every((k) => b[k] === undefined)
    // SPEC §4.4a：codex `/model`、`/fast` 執行中可換（daemon 操作 TUI 再回讀），不用重啟。
    // #393：codex 忙的時候不重啟——排到下次 idle 再套（daemon lifecycle/deferred_live.rs）；mock 4 秒後演「閒下來了」。
    let deferred = false
    if (needs_restart && bot.kind === 'codex' && within('model', 'effort', 'fast')) {
      if (run && (run.agent_status === 'working' || run.agent_status === 'blocked')) {
        deferred = true
        const runId = run.id
        setTimeout(() => {
          const r = this.runs.find((x) => x.id === runId)
          if (!r) return
          r.runtime_model = bot.model
          r.runtime_effort = bot.effort
          r.runtime_fast = bot.fast === 1
          this.emitBotStatus(id)
        }, 4000)
      } else needs_restart = false
    }
    if (needs_restart && bot.kind === 'grok' && only('effort') && bot.effort) needs_restart = false
    if (needs_restart && bot.kind === 'grok' && (only('model') || only('model', 'effort')) && bot.model) needs_restart = false
    if (needs_restart && bot.kind === 'claude' && only('model') && bot.model) needs_restart = false
    if (needs_restart && bot.kind === 'claude' && only('effort') && bot.effort) needs_restart = false
    // SPEC §4.4a：當場套用成功才更新 runtime，否則標題列會卡著「需重啟」。
    if (!needs_restart && run) {
      if (b.model !== undefined) run.runtime_model = bot.model
      if (b.effort !== undefined) run.runtime_effort = bot.effort
      if (b.fast !== undefined) run.runtime_fast = bot.fast === 1
    }
    if (run) this.emitBotStatus(id)
    const touchedLive = bot.kind === 'codex' && run !== undefined && within('model', 'effort', 'fast') && ['model', 'effort', 'fast'].some((k) => b[k] !== undefined)
    return {
      needs_restart,
      ...(touchedLive ? { live_apply: { fields: ['fast'], applied: !deferred, deferred, reason: deferred ? 'slash_gate: agent_busy' : null } } : {}),
    }
  }

  private restart(botId: string) {
    const run = this.activeRun(botId)
    if (run) {
      const inFlight = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
      if (inFlight) this.updateTurn(inFlight, { status: 'failed', completed_at: now() })
      run.state = 'stopped'
      setAgentStatus(run, 'unknown')
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

  /** SPEC §6.9，規則照 `daemon/src/bulk_restart.rs`；進度用 setTimeout 拉開，否則進度條一閃而過。 */
  private restartIdle() {
    const batch_id = ulid('batch')
    const planned: { bot_id: string; name: string }[] = []
    const skipped: { bot_id: string; name: string; reason: string; reason_label: string }[] = []
    for (const bot of this.bots) {
      if (bot.kind !== 'claude') continue
      const run = this.activeRun(bot.id)
      if (!run?.update_notice) continue
      const inFlight = this.turns.some((t) => t.run_id === run.id && t.status === 'in_flight')
      const why =
        run.state !== 'running'
          ? ['not_running', '還在啟動或關閉中']
          : run.agent_status === 'working'
            ? ['working', '正在跑，重啟會把這一回合砍掉']
            : run.agent_status === 'blocked'
              ? ['blocked', '卡在提問，等人回答']
              : run.agent_status !== 'idle'
                ? ['unknown_status', '狀態不明，不確定它在不在忙']
                : inFlight
                  ? ['turn_in_flight', '還有一回合沒收掉']
                  : null
      if (why) skipped.push({ bot_id: bot.id, name: bot.name, reason: why[0], reason_label: why[1] })
      else planned.push({ bot_id: bot.id, name: bot.name })
    }
    const total = planned.length
    const ok: { bot_id: string; name: string; run_id: string }[] = []
    planned.forEach((t, i) => {
      setTimeout(
        () => {
          this.emit('bots_restart_progress', { batch_id, index: i + 1, total, ...t, status: 'restarting' })
        },
        600 + i * 1600,
      )
      setTimeout(
        () => {
          const run_id = this.restart(t.bot_id).run_id
          ok.push({ ...t, run_id })
          this.emit('bots_restart_progress', { batch_id, index: i + 1, total, ...t, status: 'ok' })
          if (ok.length === total) this.emit('bots_restart_done', { batch_id, ok, failed: [], skipped })
        },
        1400 + i * 1600,
      )
    })
    if (total === 0) setTimeout(() => this.emit('bots_restart_done', { batch_id, ok: [], failed: [], skipped }), 300)
    return { batch_id, total, planned, skipped }
  }

  /** 有 Run 先 stop；對話歷史保留。 */
  private deleteBot(id: string) {
    this.bot(id)
    const run = this.activeRun(id)
    if (run) {
      const inFlight = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
      if (inFlight) this.updateTurn(inFlight, { status: 'failed', completed_at: now() })
      run.state = 'stopped'
      setAgentStatus(run, 'unknown')
      run.ended_at = now()
      this.emitBotStatus(id)
    }
    this.bots = this.bots.filter((x) => x.id !== id)
    // 對話歷史刻意保留。
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
      // 照 herdr 短 pane id 形狀（非 ULID），才驗得出標題列擠不擠。
      pane_id: `w${this.runs.length + 1}:p${String.fromCharCode(65 + (this.runs.length % 26))}`,
      own_tab: false,
      adopted: 0,
      native_session_id: null,
      transcript_path: null,
      status: null,
      status_line: null,
      update_notice: null,
      turn_error: null,
      runtime_model: bot.model,
      runtime_effort: bot.effort,
      runtime_fast: bot.fast === 1,
      runtime_identity: bot.identity ?? '',
      agent_status_since: now(),
      started_at: now(),
      ended_at: null,
    }
    this.runs.push(run)
    this.emitBotStatus(botId)
    setTimeout(() => {
      if (run.state !== 'starting') return
      run.state = 'running'
      setAgentStatus(run, 'idle')
      run.native_session_id = ulid('sess')
      // Only claude ships a statusLine hook; ChatPanel rebuilds others' from the store.
      if (this.bot(botId).kind === 'claude') {
        run.status = claudeStatusJson(this.projects.find((p) => p.id === this.bot(botId).project_id)?.path ?? '~')
        run.status_line = 'tony… | OP5 | 26% | 5h 85% | 7d 27% | $18.67'
        // 帶 update_notice，演 UpdateBadge。
        if (this.bot(botId).name === 'am-claude') run.update_notice = 'Update installed · Restart to update'
        // 這顆在忙，批次重啟會跳過（SPEC §6.9）。
        if (this.bot(botId).name === 'am-claude-2') {
          run.update_notice = 'Update installed · Restart to update'
          setAgentStatus(run, 'working')
        }
        // 帶 turn_error，演 TurnErrorBadge。
        if (this.bot(botId).name === 'am-claude') {
          run.turn_error = 'API Error: Connection lost mid-response. The response above may be incomplete.'
        }
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

  /** 預覽模式（issue #253）：starting 1.2 秒後轉 running，跟 daemon 的 `preview_changed` 同形。 */
  private previews = new Map<string, { status: string; port: number | null; dir: string | null; pane_id: string | null; error: string | null; started_at: string | null; source?: string | null; pid?: number | null; command?: string | null }>()

  private previewOf(botId: string) {
    this.bot(botId)
    return { ...(this.previews.get(botId) ?? { status: 'off' }), ...this.previewHints(botId) }
  }

  /** v2：候選目錄與本機在跑的 dev server（v4 含 kind／指令；只在沒接上／沒起時有意義）。 */
  private previewHints(botId: string) {
    const base = this.bot(botId).cwd ?? '/Users/m4p/project/agents-manager'
    const root = this.projects.find((p) => p.id === this.bot(botId).project_id)?.path ?? base
    return {
      // am-claude-2 演「偵測不到 dev server、只有 others」（wits-ops 的情境）。
      candidates: this.bot(botId).name === 'am-claude-2' ? [] : [{ dir: `${base}/web`, command: 'bunx vite' }, { dir: `${base}/apps/web`, command: 'bun run dev' }],
      others: [
        { port: 5241, dir: `${base}/web`, pid: 4101, relation: 'same_dir', kind: 'vite', repo: 'agents-manager' },
        { port: 6006, dir: `${root}/web`, pid: 4102, relation: 'same_dir', kind: 'storybook', repo: 'agents-manager' },
        { port: 3000, dir: `${root}/apps/admin-bff-hono`, pid: 4103, relation: 'same_dir', kind: 'unknown', repo: 'agents-manager' },
        { port: 5556, dir: `${root}/apps/admin-bff-hono`, pid: 4104, relation: 'same_dir', kind: 'unknown', repo: 'agents-manager' },
        { port: 4000, dir: `${root}/apps/ffi-server`, pid: 4105, relation: 'same_dir', kind: 'unknown', repo: 'agents-manager' },
        { port: 5173, dir: '/Users/m4p/project/agents-manager-main/web', pid: 4242, relation: 'same_repo', kind: 'vite', repo: 'agents-manager' },
        { port: 3001, dir: '/Users/m4p/project/hermes-agents/projects/wt/webui/apps/web', pid: 4377, relation: 'other', kind: 'vite', repo: 'hermes-agents' },
        { port: 3200, dir: '/Users/m4p/project/hermes-agents/projects/wt/witsper-ops', pid: 44112, relation: 'other', kind: 'next', repo: 'witsper-ops' },
      ],
    }
  }

  private setPreviewState(botId: string, patch: Record<string, unknown>) {
    const cur = this.previews.get(botId) ?? { status: 'off', port: null, dir: null, pane_id: null, error: null, started_at: null }
    const next = { ...cur, ...patch }
    this.previews.set(botId, next)
    this.emit('preview_changed', { bot_id: botId, status: next.status, port: next.port })
    return next
  }

  private startPreview(botId: string, body: Record<string, unknown> = {}) {
    const bot = this.bot(botId)
    if (bot.parent_bot_id || bot.managed_by === 'child') {
      throw new ApiError(409, { reason: 'not_top_level' }, 'not_top_level')
    }
    const cur = this.previews.get(botId)
    if (cur && (cur.status === 'starting' || cur.status === 'running')) return cur
    if (bot.name === 'am-claude-2' && body.mode !== 'attach') {
      const b = bot.cwd ?? '/Users/m4p/project/agents-manager'
      const tried = [b, `${b}/web`, ...['web', 'admin', 'docs', 'site', 'ui'].flatMap((n) => [`${b}/apps/${n}`, `${b}/packages/${n}`])].map((d) => `${d}/vite.config.{ts,mts,js,mjs} 或 package.json 的 dev script`)
      throw new ApiError(409, { reason: 'no_vite_config', tried }, 'no_vite_config')
    }
    if (body.mode === 'attach') {
      const port = Number(body.port)
      const other = this.previewHints(botId).others.find((o) => o.port === port)
      if (!other) throw new ApiError(409, { reason: 'not_dev_server' }, 'not_dev_server')
      return { ...this.setPreviewState(botId, { status: 'running', port, dir: other.dir, pane_id: null, error: null, started_at: now(), source: 'attached', pid: other.pid }), ...this.previewHints(botId) }
    }
    const used = new Set([...this.previews.values()].map((p) => p.port))
    let port = 5180
    while (used.has(port)) port += 1
    const dir = typeof body.dir === 'string' && body.dir ? body.dir : `${bot.cwd ?? '/Users/m4p/project/agents-manager'}/web`
    const starting = this.setPreviewState(botId, { status: 'starting', port, dir, pane_id: `mock-pv-${port}`, error: null, started_at: now(), source: 'spawned', pid: null, command: 'bunx vite' })
    setTimeout(() => {
      if (this.previews.get(botId)?.status === 'starting') this.setPreviewState(botId, { status: 'running' })
    }, 1200)
    return starting
  }

  private stopPreview(botId: string) {
    this.bot(botId)
    this.previews.delete(botId)
    this.emit('preview_changed', { bot_id: botId, status: 'off', port: null })
    return { status: 'off' }
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
      setAgentStatus(run, 'unknown')
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
    setAgentStatus(run, 'idle')
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

  /** 不同於 `interrupt`：沒有 active run 也不是錯誤，掛著的回合照收。 */
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
      setAgentStatus(run, 'idle')
      this.emitBotStatus(botId)
    }
    return { aborted: stuck.map((t) => t.id), keys_sent: Boolean(run), key_error: run ? null : 'no active run' }
  }

  /** 擋下理由與 daemon 同一組 key；codex 沒有 TUI 登入指令，所以 400 而非 409。 */
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
    if (!run && b.start_if_stopped === true) return this.promptStarting(botId, b)
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
    // 插隊送出（issue #103）：只有 claude 有那顆鍵，其他 kind 照舊 409 並說明為什麼沒插隊。
    // mock 不模擬版本閘門（沒有 statusLine），只分 kind——版本那條由 daemon 的單元測試把關。
    const wantSendNow = b.send_now === true
    const canSendNow = wantSendNow && this.bot(botId)?.kind === 'claude'
    if (busy && !canSendNow) {
      const why = wantSendNow
        ? {
            send_now_refused: 'send_now_unsupported_kind',
            send_now_message: '插隊送出只有 claude 有（2.1.275 的 send-now 鍵）；這顆 bot 照舊排隊。',
          }
        : {}
      throw new ApiError(409, { error: 'conflict', reason: 'a turn is already in flight', turn_id: busy.id, ...why }, 'conflict')
    }
    if (busy) {
      this.updateTurn(busy, { status: 'failed', completed_at: now() })
      this.addMessage({
        conversation_id: busy.conversation_id,
        turn_id: busy.id,
        bot_id: botId,
        role: 'system',
        content: '被插隊送出打斷（claude send-now）',
        source: 'system',
        incomplete: 0,
      })
    }
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
    setAgentStatus(run, 'working')
    this.emitBotStatus(botId)

    const lowered = text.toLowerCase()
    if (lowered.includes('blocked') || lowered.includes('rm -rf')) {
      setTimeout(() => this.enterBlocked(botId), 900)
    } else if (lowered.includes('retry')) {
      // CLI retrying upstream: turn stays in flight, only signal is `turn_progress.alert`.
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
      // A few `turn_progress` frames before the final reply.
      // 「圖片」：演對話裡的本機圖片（專案內顯示、專案外只寫路徑）。
      const reply =
        replyOverride ??
        (lowered.includes('圖片')
          ? '截圖在這：\n\n![menu](docs/screenshots/project-mem/project-mem-390-menu.png)\n\n專案外的：![](/tmp/missing-shot.png)'
          : this.nextReply())
      const slow = lowered.includes('slow')
      // 「outbox」：演 bot 在回合裡放檔，回合結束那一刻清單就該出現（不必切頁或按 ↻）。
      if (lowered.includes('outbox')) {
        const list = this.outboxAdded.get(botId) ?? []
        list.push(`download-test-${list.length + 1}.txt`)
        this.outboxAdded.set(botId, list)
      }
      const frames = slow ? 4 : 3
      // Thinking phase: `activity` only, empty `text`. Real verb is random; only the counter shape matters.
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

  /**
   * Mirrors `daemon/src/lifecycle/start_send.rs`（issue #122）：bot 沒在跑時先收下（`queued`＋`awaits_start`），
   * 再替它啟動，起來、閒下來才送。文字含「起不來」演啟動失敗：原因寫在 turn 上，按「重新啟動」（`POST /start`）照樣會送。
   */
  private promptStarting(botId: string, b: Rec) {
    const clientRequestId = typeof b.client_request_id === 'string' ? b.client_request_id : null
    const dup = clientRequestId ? this.turns.find((t) => t.client_request_id === clientRequestId) : undefined
    if (dup) return { turn_id: dup.id, message_id: null, delivery: dup.status === 'queued' ? 'queued' : dup.delivery }
    if (this.turns.some((t) => t.bot_id === botId && t.status === 'queued')) {
      throw new ApiError(409, { error: 'conflict', reason: 'a turn is already queued for this bot' }, 'conflict')
    }
    const text = String(b.text ?? '')
    const turn: MockTurn = {
      id: ulid('turn'),
      conversation_id: this.conv(botId),
      run_id: '',
      bot_id: botId,
      origin: 'web',
      status: 'queued',
      delivery: 'pending',
      client_request_id: clientRequestId,
      created_at: now(),
      completed_at: null,
      awaits_start: 1,
      start_error: null,
    }
    this.turns.push(turn)
    const msg = this.addMessage({
      conversation_id: turn.conversation_id,
      turn_id: turn.id,
      bot_id: botId,
      role: 'user',
      content: text,
      source: 'web',
      incomplete: 0,
    })
    this.emit('turn_updated', { bot_id: botId, turn })
    setTimeout(() => {
      if (turn.status !== 'queued') return
      if (text.includes('起不來')) {
        this.updateTurn(turn, { start_error: '本機上找不到 `claude` 執行檔（mock 演啟動失敗）' })
        return
      }
      try {
        this.start(botId)
      } catch {
        // 別人先起了：一樣是起來了。
      }
      this.flushWaiting(botId)
    }, 600)
    return { turn_id: turn.id, message_id: msg.id, delivery: 'queued' }
  }

  /** 起來、閒下來就把等著的那一則送出（daemon 的佇列 flush）。 */
  private flushWaiting(botId: string) {
    const turn = this.turns.find((t) => t.bot_id === botId && t.status === 'queued' && t.awaits_start === 1)
    if (!turn) return
    const run = this.activeRun(botId)
    if (!run) return
    if (run.state !== 'running' || run.agent_status !== 'idle') {
      setTimeout(() => this.flushWaiting(botId), 300)
      return
    }
    this.updateTurn(turn, { status: 'in_flight', run_id: run.id, delivery: 'ok', start_error: null })
    setAgentStatus(run, 'working')
    this.emitBotStatus(botId)
    setTimeout(() => this.finishTurn(botId, turn, 'hook'), 1500)
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
      setAgentStatus(run, 'idle')
      this.emitBotStatus(botId)
    }
  }

  /** Public so the dev helper can force a blocked state without a prompt. */
  enterBlocked(botId: string) {
    const run = this.activeRun(botId)
    if (!run) return
    setAgentStatus(run, 'blocked')
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
        setAgentStatus(run, 'working')
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

  /** Enter 是分開的一顆鍵。 */
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
    setAgentStatus(run, 'idle')
    this.emitBotStatus(botId)
  }

  /** Mirrors `start_send::withdraw_turn`：只撤還在等 bot 起來的那一則，其他 409。 */
  private withdraw(turnId: string) {
    const turn = this.turns.find((t) => t.id === turnId)
    if (!turn) throw new ApiError(404, { reason: 'turn not found' }, 'not found')
    if (turn.status !== 'queued' || turn.awaits_start !== 1) {
      throw new ApiError(409, { error: 'conflict', reason: 'turn is not waiting for its bot to start', turn_id: turnId, status: turn.status }, 'conflict')
    }
    this.updateTurn(turn, { status: 'failed', delivery: 'failed', completed_at: now() })
    this.addMessage({
      conversation_id: turn.conversation_id,
      turn_id: turn.id,
      bot_id: turn.bot_id,
      role: 'system',
      content: '使用者取消了這一則：它是 bot 沒在跑時送的，還在等 bot 起來，沒有送出，不會再送。',
      source: 'system',
      incomplete: 0,
    })
    return { ok: true }
  }

  private abandon(turnId: string) {
    const turn = this.turns.find((t) => t.id === turnId)
    if (!turn) throw new ApiError(404, { reason: 'turn not found' }, 'not found')
    this.updateTurn(turn, { status: 'failed', completed_at: now() })
    this.setIdle(turn.bot_id)
    return { ok: true }
  }

  /**
   * Mirrors `daemon/src/api.rs::messages`：插入順序倒序分頁（`before` = 目前最舊一則的 id），
   * `turn_id`／`role` 在同一段對話裡先過濾再分頁，回傳的 `messages` 已依時間正序。
   * 不吃這些參數的話，issue #25 的往前翻頁與群組未讀的回合確認在 mock 下永遠走不到真 daemon 那條路。
   */
  private messagesOf(botId: string, q: URLSearchParams) {
    const limit = Math.min(500, Math.max(1, Number(q.get('limit') ?? 100) || 100))
    const before = q.get('before') ?? ''
    const turnId = q.get('turn_id') ?? ''
    const role = q.get('role') ?? ''
    // 靜默忽略會讓呼叫端以為過濾過了，拿整段當成某個 role 的全部（同 daemon 回 400）。
    if (role && !['user', 'assistant', 'system'].includes(role)) {
      throw new ApiError(400, { error: 'bad_request', message: `bad role \`${role}\`` }, 'bad request')
    }
    const mine = this.messages.filter((m) => m.bot_id === botId)
    let upto = mine.length
    if (before) {
      const at = mine.findIndex((m) => m.id === before)
      if (at < 0) throw new ApiError(400, { error: 'bad_request', message: `before message \`${before}\` not found` }, 'bad request')
      upto = at
    }
    const rows = mine
      .slice(0, upto)
      .filter((m) => (!turnId || m.turn_id === turnId) && (!role || m.role === role))
    return {
      bot_id: botId,
      conversation_id: this.conv(botId),
      messages: rows.slice(Math.max(0, rows.length - limit)),
      turns: this.turns.filter((t) => t.bot_id === botId),
      has_more: rows.length > limit,
    }
  }

  // group chat (SPEC §13)

  /** Mirrors `daemon/src/group.rs::messages`. */
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

  /** Mirrors `daemon/src/group.rs::chat`. */
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

  // 主機 shell

  /** 有真的行緩衝，`shellText` 依指令追加輸出，mock 下才試得出輸入框與按鍵列。 */
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

  /**
   * 白名單兩份（跟 daemon 的 `shell::registered` 一樣，§6.5e）：自己開的那幾顆，加上被 trace 的 pane。
   * 被 trace 的**每一次**都照 pane 表判斷：有 listen port 的只能看，打字回 403；看過一次不會變成自己開的。
   */
  private shell(host: string, paneId: string, access: 'view' | 'type' = 'view'): MockShell {
    const key = `${host}/${paneId}`
    const s = this.shells.get(key)
    if (s) return s
    const traced = this.allPanes().find((x) => x.host === host && x.pane_id === paneId)
    if (!traced) throw new ApiError(404, { error: 'not_found', what: 'shell' }, 'shell not found')
    if (traced.read_only && access === 'type') {
      throw new ApiError(403, { error: 'read_only_pane', kind: traced.kind, message: '這顆 pane 開著 port（dev server 之類），只能看不能打字' }, 'read only')
    }
    const screen = this.tracedScreens.get(key)
    if (screen) return screen
    const cwd = String(traced.cwd ?? '/Users/me')
    const made: MockShell = {
      host,
      workspace_id: String(traced.workspace_id ?? 'w1'),
      tab_id: String(traced.tab_id ?? 'w1:t1'),
      pane_id: paneId,
      cwd,
      created_at: String(traced.first_seen ?? new Date().toISOString()),
      lines: traced.read_only
        ? ['$ npm run dev', '', '  ▲ Next.js 15.0.0', '  - Local:   http://localhost:3010', '', ' ✓ Ready in 1.2s']
        : traced.foreground
          ? [`${cwd.split('/').pop() ?? '~'} % ${String(traced.foreground)}`, '~', '~', '"notes.md" 3L, 42B']
          : [`${cwd.split('/').pop() ?? '~'} % ls`, 'README.md  daemon  web'],
      typed: '',
    }
    this.tracedScreens.set(key, made)
    return made
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
    const s = this.shell(host, paneId, 'type')
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
      /* 只按 Enter */
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
    const s = this.shell(host, paneId, 'type')
    const list = Array.isArray(keys) ? keys.map((k) => String(k)) : []
    if (list.length === 0) throw new ApiError(400, { error: 'bad_request', message: 'keys must not be empty' }, 'bad request')
    for (const k of list) {
      if (k === 'ctrl+c') {
        s.lines.push(`${s.cwd.split('/').pop() ?? '~'} % ${s.typed}^C`, '')
        s.typed = ''
      } else if (k === 'esc' || k === 'tab') {
        /* 無效果，但不能是錯誤 */
      } else if (k === 'enter') {
        this.shellText(host, paneId, '', true)
      } else if (k === 'up' || k === 'down') {
        /* 不模擬歷史 */
      } else if (k === 'backspace') {
        s.typed = s.typed.slice(0, -1)
      } else if (k === 'space') {
        // herdr 的鍵名：空白要寫 `space`（`usePaneKeys.herdrKeyFromEvent` 實測過）。
        s.typed += ' '
      } else if (Array.from(k).length === 1) {
        // 鍵盤同步模式下每一下按鍵都是一個單字元鍵；不回顯的話 mock 看起來像壞掉。
        s.typed += k
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
    // SPEC §4.4a：codex 狀態列顯示啟動時的值（非 DB 設定），所以從 run 的 runtime 值畫。
    const codexStatusLine =
      bot.kind === 'codex' && run
        ? [
            '',
            [run.runtime_model ?? 'gpt-5.6-luna', run.runtime_effort ?? '', run.runtime_fast ? 'fast' : '']
              .filter(Boolean)
              .join(' ') + ` · ${bot.cwd ?? '~'} · Context 0% used · 5h 100% left`,
          ]
        : []
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
    const all = [...head, ...body, ...codexStatusLine]
    const text = all.slice(Math.max(0, all.length - lines)).join('\n')
    return {
      text,
      revision: this.seq,
      truncated: all.length > lines,
      source,
      pane_id: run?.pane_id ?? null,
      columns: run ? this.paneColumns(run) : null,
      rows: run ? 27 : null,
    }
  }

  // dev helpers

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
    // SPEC §11.6; `connected` kept for the pre-§11 shape.
    this.emit('daemon_status', { herdr_connected: v, connected: v, default_connected: false, hosts: this.hostMap() })
    for (const b of this.bots) this.emitBotStatus(b.id)
  }

  botIdByName(name: string): string | undefined {
    return this.bots.find((b) => b.name === name)?.id
  }

  setForeignPanes(n: number) {
    this.foreignPanes = Math.max(0, Math.floor(n))
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
    // 窄 pane / 移到自己的分頁
    paneSqueeze: (n = 5) => mock.setForeignPanes(n),
    // 下一個符合的請求回錯：`__amMock.failNext('POST', 'identities/.*/login', 409, {reason: '…', message: '…'})`
    failNext: (method: HttpMethod, pattern: string, status: number, body: Rec = {}) => mock.failNext(method, pattern, status, body),
  }
}
