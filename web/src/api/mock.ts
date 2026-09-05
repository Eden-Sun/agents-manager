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
 * Dev helpers on `window.__amMock`: `resync()`, `dropSocket()`, `block(botId)`, `disconnect()`.
 */

import { ApiError } from './types'
import type { HttpMethod, SocketHandlers, Transport } from './transport'

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
  created_at: string
}

interface MockBot {
  id: string
  project_id: string
  name: string
  kind: 'claude' | 'codex'
  args_json: string
  autostart: number
  inject_hooks: number
  auto_approve: number
  created_at: string
}

interface MockProject {
  id: string
  path: string
  label: string
  workspace_id: string | null
  created_at: string
}

const REPLIES = [
  '好的，我看了一下 `src/api/transport.ts`：token 會在 `GET /api/session` 之後快取，後續 REST 都帶 `X-AM-Token`。\n\n需要我把重連的退避上限調整成 30 秒嗎？',
  '已完成。修改重點：\n\n1. `runs_one_active` 部分唯一索引避免重複啟動\n2. per-bot mutex 包住 start / stop / prompt\n3. hook 早於 RPC 回應時仍能配對 in-flight Turn\n\n測試都過了。',
  '這段的問題在於 `agent.start` 是非同步的，socket 立刻回 `launch_pending:true`，所以必須接 `agent.wait {until:[idle,done,blocked]}` 才能確定就緒。',
  'PONG',
]

export class MockTransport implements Transport {
  readonly mock = true

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
      created_at: now(),
    }
    this.projects.push(p)
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-claude',
      kind: 'claude',
      args_json: JSON.stringify(['--model', 'opus']),
      autostart: 1,
      inject_hooks: 1,
      auto_approve: 1,
      created_at: now(),
    })
    this.bots.push({
      id: ulid('bot'),
      project_id: p.id,
      name: 'am-codex',
      kind: 'codex',
      args_json: '[]',
      autostart: 0,
      inject_hooks: 1,
      auto_approve: 1,
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
    if (method === 'GET' && rawPath === '/fs/dirs') return this.dirs(q.get('path') ?? '')

    if (method === 'POST' && rawPath === '/projects') return this.addProject(b)
    if (method === 'DELETE' && seg[0] === 'projects' && seg.length === 2) return this.deleteProject(seg[1])
    if (method === 'POST' && seg[0] === 'projects' && seg[2] === 'bots') return this.addBot(seg[1], b)

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
        if (action === 'interrupt') return this.interrupt(botId)
        if (action === 'prompt') return this.prompt(botId, b)
        if (action === 'keys') return this.keys(botId, b)
      }
    }

    if (method === 'POST' && seg[0] === 'turns' && seg[2] === 'abandon') return this.abandon(seg[1])

    throw new ApiError(404, { reason: `mock: no route for ${method} ${rawPath}` }, 'not found')
  }

  // ------------------------------------------------------------------ helpers

  private dirs(path: string) {
    const home = '/Users/me'
    const tree: Record<string, string[]> = {
      '/': ['Users', 'opt', 'tmp'],
      '/Users': ['me'],
      '/Users/me': ['project', 'Documents', 'Downloads'],
      '/Users/me/project': ['foo', 'bar', 'agents-manager'],
      '/Users/me/project/foo': ['src'],
      '/Users/me/Documents': [],
      '/Users/me/Downloads': [],
    }
    const cur = path && path in tree ? path : path.startsWith('/Users/me/project/') ? path : home
    const kids = tree[cur] ?? []
    const parent = cur === '/' ? null : cur.slice(0, cur.lastIndexOf('/')) || '/'
    return {
      path: cur,
      parent,
      home,
      entries: kids.map((name) => ({
        name,
        path: cur === '/' ? `/${name}` : `${cur}/${name}`,
        git: name === 'foo' || name === 'agents-manager',
      })),
    }
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

  private addMessage(m: Omit<MockMessage, 'id' | 'created_at'>): MockMessage {
    const msg: MockMessage = { ...m, id: ulid('msg'), created_at: now() }
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
      projects: this.projects.map((p) => ({
        id: p.id,
        path: p.path,
        label: p.label,
        workspace_id: p.workspace_id,
        bots: this.bots
          .filter((b) => b.project_id === p.id)
          .map((b) => {
            const run = this.activeRun(b.id) ?? null
            return {
              id: b.id,
              project_id: b.project_id,
              name: b.name,
              kind: b.kind,
              args: JSON.parse(b.args_json) as string[],
              autostart: b.autostart === 1,
              inject_hooks: b.inject_hooks === 1,
              auto_approve: b.auto_approve === 1,
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
    const p: MockProject = {
      id: ulid('proj'),
      path: canonical,
      label: String(b.label ?? '').trim() || (canonical.split('/').pop() ?? canonical),
      workspace_id: null,
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
    if (!/^[a-z][a-z0-9_-]{0,31}$/.test(name)) {
      throw new ApiError(400, { reason: 'name 必須符合 [a-z][a-z0-9_-]{0,31}' }, 'bad request')
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
      kind: b.kind === 'codex' ? 'codex' : 'claude',
      args_json: JSON.stringify(Array.isArray(b.args) ? b.args : []),
      autostart: b.autostart ? 1 : 0,
      inject_hooks: 1,
      auto_approve: 1,
      created_at: now(),
    }
    this.bots.push(bot)
    this.emit('bot_changed', { bot_id: bot.id })
    return { bot_id: bot.id }
  }

  private patchBot(id: string, b: Rec) {
    const bot = this.bot(id)
    if (b.name !== undefined && this.activeRun(id)) {
      throw new ApiError(409, { reason: '有 active Run 時不可改名' }, 'conflict')
    }
    if (typeof b.name === 'string') bot.name = b.name
    if (Array.isArray(b.args)) bot.args_json = JSON.stringify(b.args)
    if (b.autostart !== undefined) bot.autostart = b.autostart ? 1 : 0
    this.emit('bot_changed', { bot_id: id })
    return bot
  }

  private deleteBot(id: string) {
    if (this.activeRun(id)) throw new ApiError(409, { reason: 'Bot 尚未停止' }, 'conflict')
    this.bots = this.bots.filter((x) => x.id !== id)
    this.emit('bot_changed', { bot_id: id, deleted: true })
    return null
  }

  private start(botId: string) {
    this.bot(botId)
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

  private prompt(botId: string, b: Rec) {
    const run = this.activeRun(botId)
    if (!run || run.state !== 'running') {
      throw new ApiError(409, { reason: 'Run 不在 running 狀態' }, 'conflict')
    }
    if (run.agent_status === 'blocked') {
      throw new ApiError(409, { reason: 'agent 處於 blocked，請先在終端面板回應' }, 'conflict')
    }
    const clientRequestId = typeof b.client_request_id === 'string' ? b.client_request_id : null
    if (clientRequestId) {
      const dup = this.turns.find((t) => t.client_request_id === clientRequestId)
      if (dup) return { turn_id: dup.id, message_id: null, delivery: dup.delivery }
    }
    const busy = this.turns.find((t) => t.run_id === run.id && t.status === 'in_flight')
    if (busy) throw new ApiError(409, { reason: '已有進行中的 Turn', turn_id: busy.id }, 'conflict')
    const unknown = this.turns.find((t) => t.run_id === run.id && t.delivery === 'unknown')
    if (unknown) {
      throw new ApiError(409, { reason: 'delivery=unknown，需先 abandon 或 stop', turn_id: unknown.id }, 'conflict')
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
      setTimeout(() => this.finishTurn(botId, turn, 'hook'), lowered.includes('slow') ? 8000 : 1900)
    }
    return { turn_id: turn.id, message_id: userMsg.id, delivery: 'ok' }
  }

  private finishTurn(botId: string, turn: MockTurn, source: 'hook' | 'terminal_fallback') {
    if (turn.status !== 'in_flight') return
    const run = this.activeRun(botId)
    const reply = REPLIES[this.replyIndex++ % REPLIES.length]
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
    this.emit('daemon_status', { connected: v })
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
  }
}
