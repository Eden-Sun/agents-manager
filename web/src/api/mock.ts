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
 * `hostDown(name)`, `hostUp(name)` (SPEC §11.6 remote hosts).
 */

import { parseMentions } from './mentions'
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

interface MockRun {
  id: string
  bot_id: string
  state: 'starting' | 'running' | 'stopping' | 'stopped' | 'exited'
  agent_status: 'idle' | 'working' | 'blocked' | 'unknown'
  workspace_id: string | null
  pane_id: string | null
  adopted: number
  native_session_id: string | null
  transcript_path: string | null
  started_at: string
  ended_at: string | null
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
  { number: 41, title: '即時輸出前幾幀是 TUI 雜訊（✢ Improvising…）', state: 'open', labels: [{ name: 'daemon', color: '1d76db' }, { name: 'polish', color: 'fbca04' }], author: 'edansun', updated_at: inHours(-9), body: '`turn_progress` 的前 1–2 幀會帶 Claude Code 的 spinner 文字，應在 daemon 端過濾。' },
  { number: 40, title: 'Bot 設定：codex 模型 / effort / fast 從 `GET /api/models` 取', state: 'open', labels: [{ name: 'enhancement', color: 'a2eeef' }, { name: 'web', color: '5319e7' }], author: 'edansun', updated_at: inHours(-20), body: '目前是靜態清單。改為 API 驅動，失敗退回靜態清單。' },
  { number: 39, title: '主機面板顯示三個 kind 的工具偵測徽章', state: 'open', labels: [{ name: 'enhancement', color: 'a2eeef' }], author: 'm4p-bot', updated_at: inHours(-30), body: '已安裝 ✓ / 未安裝 ✗ / 未登入 !，附「安裝」按鈕。' },
  { number: 38, title: '額度徽章：低於 20% 用警示色', state: 'open', labels: [{ name: 'web', color: '5319e7' }, { name: 'good first issue', color: '7057ff' }], author: 'edansun', updated_at: inHours(-50), body: '剩餘 = 100 − used_pct。' },
  { number: 37, title: '長訊息不要預先收合', state: 'closed', labels: [{ name: 'web', color: '5319e7' }], author: 'edansun', updated_at: inHours(-60), body: '已在 v3.9 UI polish 移除 isLong / clamped。' },
  { number: 35, title: 'hash 式 herdr agent name', state: 'closed', labels: [{ name: 'daemon', color: '1d76db' }], author: 'edansun', updated_at: inHours(-100), body: '`<project slug>-<bot id 尾 6 碼>`。' },
  { number: 33, title: 'grok kind：hook 不走 argv', state: 'closed', labels: [{ name: 'daemon', color: '1d76db' }, { name: 'grok', color: '000000' }], author: 'edansun', updated_at: inHours(-140), body: '改寫入 `<GROK_HOME>/hooks/agents-manager.json`。' },
]

/** SPEC §11.2 `[[hosts]]` + the runtime connection state the daemon reports. */
interface MockHost {
  name: string
  ssh: string
  ssh_port: number
  herdr_session: string
  remote_path: string
  hook_port: number
  connected: boolean
  error: string | null
  /** v4.0 tool detection on that host */
  tools: Record<BotKind, MockTool>
}

interface MockTool {
  installed: boolean
  path: string | null
  version: string | null
  logged_in: boolean | null
}

const TOOLS_ALL_OK: Record<BotKind, MockTool> = {
  claude: { installed: true, path: '/opt/homebrew/bin/claude', version: '2.1.40', logged_in: true },
  codex: { installed: true, path: '/opt/homebrew/bin/codex', version: '0.68.0', logged_in: true },
  grok: { installed: true, path: '/Users/me/.local/bin/grok', version: '1.0.13', logged_in: null },
}

/** v4.0 `GET /api/models` catalogue (what the CLIs report on 2026-09-06). */
const MODELS: Record<BotKind, Rec[]> = {
  claude: [
    { id: 'opus', display_name: 'Opus', description: '最強推理', is_default: false, default_effort: null, efforts: [], service_tiers: [] },
    { id: 'sonnet', display_name: 'Sonnet', description: '速度與品質平衡', is_default: true, default_effort: null, efforts: [], service_tiers: [] },
    { id: 'haiku', display_name: 'Haiku', description: '最快、最省', is_default: false, default_effort: null, efforts: [], service_tiers: [] },
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
    { id: 'grok-4.6', display_name: 'Grok 4.6', description: 'grok CLI 預設', is_default: true, default_effort: 'medium', efforts: ['low', 'medium', 'high'], service_tiers: [] },
    { id: 'grok-4.5', display_name: 'Grok 4.5', description: '上一代', is_default: false, default_effort: 'medium', efforts: ['low', 'medium', 'high'], service_tiers: [] },
  ],
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
  private quota: Record<string, Rec | null> = {
    claude: { five_hour: { used_pct: 18, resets_at: inHours(2.4) }, seven_day: { used_pct: 40, resets_at: inHours(70) }, plan: 'Max 20x', updated_at: now() },
    'claude:cc1': { five_hour: { used_pct: 85, resets_at: inHours(1.1) }, seven_day: { used_pct: 30, resets_at: inHours(120) }, plan: 'Pro', updated_at: now() },
    codex: { five_hour: { used_pct: 63, resets_at: inHours(3.2) }, seven_day: { used_pct: 88, resets_at: inHours(41) }, plan: 'Plus', updated_at: now() },
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
  private conversations = new Map<string, string>()

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
      created_at: now(),
    })
    installDevHelpers(this)
  }

  // ---------------------------------------------------------------- transport

  session(): Promise<string> {
    return Promise.resolve('mock-ui-token')
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
      this.emit('quota_updated', { kind: 'codex', quota: q })
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
    if (method === 'GET' && rawPath === '/fs/dirs') return this.dirs(q.get('path') ?? '', q.get('host') ?? '')
    if (method === 'GET' && rawPath === '/models') return this.models(q.get('kind') ?? '', q.get('host') ?? '')
    if (method === 'GET' && rawPath === '/quota') return { kinds: this.quota }
    if (method === 'POST' && seg[0] === 'hosts' && seg[2] === 'tools' && seg[3] === 'install') return this.installTool(seg[1], b)

    if (method === 'POST' && rawPath === '/identities') return this.addIdentity(b)
    if (seg[0] === 'identities' && seg.length === 2 && method === 'DELETE') return this.deleteIdentity(decodeURIComponent(seg[1]))
    if (method === 'POST' && rawPath === '/hosts') return this.addHost(b)
    if (seg[0] === 'hosts' && seg.length === 2 && method === 'DELETE') return this.deleteHost(seg[1])
    if (seg[0] === 'hosts' && seg[2] === 'reconnect' && method === 'POST') return this.reconnectHost(seg[1])

    if (method === 'POST' && rawPath === '/projects') return this.addProject(b)
    if (method === 'DELETE' && seg[0] === 'projects' && seg.length === 2) return this.deleteProject(seg[1])
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'bots') return this.addBot(seg[1], b)
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'messages') return this.projectMessages(seg[1], q)
    if (method === 'GET' && seg[0] === 'projects' && seg[2] === 'issues') return this.issues(seg[1], seg[3], q)
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'chat') return this.projectChat(seg[1], b)

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
        if (action === 'prompt') return this.prompt(botId, b)
        if (action === 'keys') return this.keys(botId, b)
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

  /** v4.0 `GET /api/models?kind=&host=` — unknown kind → 400; a down host → 502. */
  private models(kind: string, host: string) {
    if (!BOT_KINDS.includes(kind as BotKind)) {
      throw new ApiError(400, { error: 'bad_request', message: 'kind must be claude, codex or grok' }, 'bad request')
    }
    const remote = host && host !== 'local' ? this.host(host) : null
    if (remote && !remote.connected) {
      throw new ApiError(502, { error: 'upstream', message: `主機 ${remote.name} 未連線` }, 'upstream')
    }
    return { kind, host: remote?.name ?? 'local', models: MODELS[kind as BotKind] }
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

  /** SPEC §11.5: the same JSON shape for local and remote; `host` picks the tree. */
  private dirs(path: string, host: string) {
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
          [home]: ['work', 'src', 'Documents'],
          [`${home}/work`]: ['api-server', 'web-client', 'scratch'],
          [`${home}/work/api-server`]: ['crates'],
          [`${home}/src`]: ['herdr'],
          [`${home}/Documents`]: [],
        }
      : {
          '/': ['Users', 'opt', 'tmp'],
          '/Users': ['me'],
          '/Users/me': ['project', 'Documents', 'Downloads'],
          '/Users/me/project': ['foo', 'bar', 'agents-manager'],
          '/Users/me/project/foo': ['src'],
          '/Users/me/Documents': [],
          '/Users/me/Downloads': [],
        }
    const gitDirs = remote
      ? new Set([`${home}/work/api-server`, `${home}/work/web-client`, `${home}/src/herdr`])
      : new Set(['/Users/me/project/foo', '/Users/me/project/agents-manager'])
    const cur = path && (path in tree || path.startsWith(`${home}/`)) ? path : home
    const kids = tree[cur] ?? []
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
    this.emit('identities_changed', {})
    return {}
  }

  private deleteIdentity(name: string) {
    const used = this.bots.filter((x) => x.identity === name)
    if (used.length > 0) {
      throw new ApiError(409, { error: 'conflict', reason: `仍有 ${used.length} 個 Bot 使用身份 ${name}` }, 'conflict')
    }
    this.identities = this.identities.filter((x) => x.name !== name)
    this.emit('identities_changed', {})
    return {}
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
      hook_port: Number(b.hook_port ?? 7788) || 7788,
      connected: false,
      error: null,
      // A fresh remote box: claude + codex present, grok missing (exercises the tools hint).
      tools: { claude: { ...TOOLS_ALL_OK.claude }, codex: { ...TOOLS_ALL_OK.codex }, grok: { installed: false, path: null, version: null, logged_in: null } },
    }
    // API.md: an existing name is an update (disconnect, then reconnect with the new config).
    this.hosts = this.hosts.filter((x) => x.name !== name)
    this.hosts.push(h)
    await sleep(700) // ssh master + remote `herdr session list` take a moment
    this.dial(h)
    this.emit('host_changed', { name: h.name, connected: h.connected, error: h.error })
    return { name: h.name, connected: h.connected, error: h.error }
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
    this.emit('host_changed', { name, connected: false, error: 'removed' })
    this.emit('project_changed', {})
    return {}
  }

  private async reconnectHost(name: string) {
    const h = this.host(name)
    await sleep(600)
    this.dial(h)
    this.emit('host_changed', { name: h.name, connected: h.connected, error: h.error })
    for (const b of this.botsOnHost(h.name)) this.emitBotStatus(b.id)
    return { name: h.name, connected: h.connected, error: h.error }
  }

  private botsOnHost(name: string): MockBot[] {
    const pids = new Set(this.projects.filter((p) => p.host === name).map((p) => p.id))
    return this.bots.filter((b) => pids.has(b.project_id))
  }

  /** Dev helper: flip a host up / down the way the daemon's health check would. */
  setHostConnected(name: string, connected: boolean) {
    const h = this.hosts.find((x) => x.name === name)
    if (!h) return
    h.connected = connected
    h.error = connected ? null : 'ssh master 已退出（mock 模擬斷線）'
    this.emit('host_changed', { name: h.name, connected: h.connected, error: h.error })
    this.emit('daemon_status', { herdr_connected: this.connected, connected: this.connected, hosts: this.hostMap() })
    for (const b of this.botsOnHost(h.name)) this.emitBotStatus(b.id)
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
  }

  private addMessage(m: Omit<MockMessage, 'id' | 'created_at' | 'group_id'> & { group_id?: string | null }): MockMessage {
    const msg: MockMessage = { group_id: null, ...m, id: ulid('msg'), created_at: now() }
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
          hook_port: null,
          connected: this.connected,
          error: null,
          attach_command: 'herdr --session agents-manager',
          tools: this.localTools,
        },
        ...this.hosts.map((h) => ({
          name: h.name,
          ssh: h.ssh,
          ssh_port: h.ssh_port,
          herdr_session: h.herdr_session,
          remote_path: h.remote_path,
          hook_port: h.hook_port,
          connected: h.connected,
          error: h.error,
          attach_command: `herdr --remote ${h.ssh}${h.ssh_port !== 22 ? ` -p ${h.ssh_port}` : ''} --session ${h.herdr_session}`,
          tools: h.tools,
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
    const needs_restart = run !== undefined && LAUNCH_FIELDS.some((k) => b[k] !== undefined)
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
      pane_id: ulid('pane'),
      adopted: 0,
      native_session_id: null,
      transcript_path: null,
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
    })
    this.updateTurn(turn, { delivery: 'ok' })
    run.agent_status = 'working'
    this.emitBotStatus(botId)

    const lowered = text.toLowerCase()
    if (lowered.includes('blocked') || lowered.includes('rm -rf')) {
      setTimeout(() => this.enterBlocked(botId), 900)
    } else if (lowered.includes('fallback')) {
      setTimeout(() => this.finishTurn(botId, turn, 'terminal_fallback'), 2600)
    } else {
      // v3.9 live output: 3–4 `turn_progress` frames (every 0.5 s) before the final reply.
      const reply = replyOverride ?? this.nextReply()
      const slow = lowered.includes('slow')
      const frames = slow ? 4 : 3
      const lines = reply.split('\n')
      // Grow by whole lines when there are several (a fence never splits mid-way), else by chars.
      const partial = (i: number) =>
        lines.length > 1
          ? lines.slice(0, Math.max(1, Math.ceil((lines.length * i) / (frames + 1)))).join('\n')
          : reply.slice(0, Math.max(1, Math.round((reply.length * i) / (frames + 1))))
      for (let i = 1; i <= frames; i++) {
        setTimeout(() => {
          if (turn.status !== 'in_flight') return
          this.emit('turn_progress', { bot_id: botId, run_id: run.id, turn_id: turn.id, text: partial(i), revision: i })
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
    return { text, revision: this.seq, truncated: all.length > lines, source }
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
    this.emit('daemon_status', { herdr_connected: v, connected: v, hosts: this.hostMap() })
    for (const b of this.bots) this.emitBotStatus(b.id)
  }

  botIdByName(name: string): string | undefined {
    return this.bots.find((b) => b.name === name)?.id
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
  }
}
