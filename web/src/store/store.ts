/**
 * Single Zustand store: server state mirrored from `GET /api/state` + `/ws`, plus UI state.
 *
 * Event flow (SPEC §7.3): the socket carries `{seq, type, data}` frames. We track the highest
 * seq and hand it back as `?since=` on reconnect; a `resync` frame (or a socket that reopens
 * with a gap) triggers a full `GET /api/state` plus a message reload for the selected bot.
 */

import { create } from 'zustand'
import * as api from '../api'
import {
  frameBotId,
  hostArray,
  lampOf,
  optStr,
  sortById,
  sortByTime,
  str,
  toMessage,
  toRun,
  toTurn,
  unwrap,
  isRec,
  bool,
  pick,
} from '../api/normalize'
import { ApiError } from '../api/types'
import type { Bot, GroupChatResult, GroupMessage, Host, HostResult, Identity, Lamp, Message, NewBotInput, NewHostInput, NewIdentityInput, NewProjectInput, PatchBotInput, Project, Run, TerminalSource, Turn } from '../api/types'

export type SocketStatus = 'connecting' | 'open' | 'closed'
export type RightTab = 'chat' | 'terminal'

export interface Notice {
  id: number
  kind: 'error' | 'info'
  text: string
}

export interface ComposerState {
  disabled: boolean
  reason: string
  inFlightTurnId: string | null
  unknownTurnId: string | null
}

interface StoreState {
  ready: boolean
  bootError: string | null
  socket: SocketStatus
  /** daemon <-> 本機 herdr link (SPEC §2.2 "連線") */
  connected: boolean
  lastSeq: number

  /** SPEC §11.6 remote hosts; the local machine is never in this list. */
  hosts: Host[]
  identities: Identity[]
  projects: Project[]
  bots: Bot[]
  runs: Record<string, Run | null>
  turns: Record<string, Record<string, Turn>>
  messages: Record<string, Message[]>
  loadedBots: Record<string, boolean>

  /**
   * SPEC §13 group view. Non-null = the right pane shows this project's group timeline
   * instead of `selectedBotId`'s conversation (the bot selection is kept for when the
   * user switches back).
   */
  selectedProjectId: string | null
  /** project_id → merged member timeline (`GET /projects/:id/messages`) plus WS appends. */
  groupMessages: Record<string, GroupMessage[]>
  loadedProjects: Record<string, boolean>
  /** §13.6: replies that arrived while that project's group view was not open (memory only). */
  groupUnread: Record<string, number>

  selectedBotId: string | null
  rightTab: RightTab
  /** 開著「Bot 設定」面板的 bot id（null = 面板關閉）。 */
  settingsBotId: string | null
  notices: Notice[]
  busy: Record<string, boolean>

  bootstrap: () => Promise<void>
  refreshState: () => Promise<void>
  selectBot: (botId: string | null) => void
  /** Open the §13 group view of a project (null = back to the selected bot). */
  selectProject: (projectId: string | null) => void
  loadGroupMessages: (projectId: string) => Promise<void>
  /** `POST /projects/:id/chat`; null = failed (reason already shown as a notice). */
  sendGroupChat: (projectId: string, text: string) => Promise<GroupChatResult | null>
  setRightTab: (tab: RightTab) => void
  openSettings: (botId: string) => void
  closeSettings: () => void
  loadMessages: (botId: string) => Promise<void>
  notify: (kind: Notice['kind'], text: string) => void
  dismiss: (id: number) => void

  startBot: (botId: string) => Promise<void>
  stopBot: (botId: string) => Promise<void>
  interruptBot: (botId: string) => Promise<void>
  sendPrompt: (botId: string, text: string) => Promise<boolean>
  sendKeys: (botId: string, keys: string[]) => Promise<void>
  abandonTurn: (botId: string, turnId: string) => Promise<void>
  addHost: (input: NewHostInput) => Promise<HostResult | null>
  addIdentity: (input: NewIdentityInput) => Promise<boolean>
  removeIdentity: (name: string) => Promise<void>
  removeHost: (name: string) => Promise<void>
  reconnectHost: (name: string) => Promise<HostResult | null>
  addProject: (input: NewProjectInput) => Promise<boolean>
  addBot: (projectId: string, input: NewBotInput) => Promise<boolean>
  /** `PATCH /api/bots/:id` — 回傳 `needs_restart`，失敗回 null（原因已跳通知）。 */
  patchBot: (botId: string, input: PatchBotInput) => Promise<boolean | null>
  restartBot: (botId: string) => Promise<boolean>
  removeBot: (botId: string) => Promise<void>
  removeProject: (projectId: string) => Promise<void>
  readTerminal: (botId: string, source: TerminalSource, lines: number) => ReturnType<typeof api.fetchTerminal>
}

let noticeSeq = 0

function errText(e: unknown): string {
  if (e instanceof ApiError) return `${e.message}（HTTP ${e.status}）`
  if (e instanceof Error) return e.message
  return String(e)
}

export const useStore = create<StoreState>((set, get) => ({
  ready: false,
  bootError: null,
  socket: 'connecting',
  connected: true,
  lastSeq: 0,

  hosts: [],
  identities: [],
  projects: [],
  bots: [],
  runs: {},
  turns: {},
  messages: {},
  loadedBots: {},

  selectedProjectId: null,
  groupMessages: {},
  loadedProjects: {},
  groupUnread: {},

  selectedBotId: null,
  rightTab: 'chat',
  settingsBotId: null,
  notices: [],
  busy: {},

  notify: (kind, text) => {
    noticeSeq += 1
    const id = noticeSeq
    set((s) => ({ notices: [...s.notices, { id, kind, text }] }))
    setTimeout(() => get().dismiss(id), kind === 'error' ? 8000 : 4000)
  },

  dismiss: (id) => set((s) => ({ notices: s.notices.filter((n) => n.id !== id) })),

  async bootstrap() {
    try {
      await api.session()
      await get().refreshState()
      set({ ready: true, bootError: null })
    } catch (e) {
      set({ ready: false, bootError: errText(e) })
      return
    }
    connectSocket(set, get)
  },

  async refreshState() {
    const st = await api.fetchState()
    const runs: Record<string, Run | null> = {}
    for (const b of st.bots) runs[b.id] = st.runs.find((r) => r.bot_id === b.id) ?? null
    set((s) => {
      const turns = { ...s.turns }
      for (const t of st.turns) {
        const botId = t.bot_id ?? st.bots.find((b) => runs[b.id]?.id === t.run_id)?.id
        if (!botId) continue
        turns[botId] = { ...(turns[botId] ?? {}), [t.id]: t }
      }
      const selected =
        s.selectedBotId && st.bots.some((b) => b.id === s.selectedBotId)
          ? s.selectedBotId
          : (st.bots[0]?.id ?? null)
      const selectedProject =
        s.selectedProjectId && st.projects.some((p) => p.id === s.selectedProjectId) ? s.selectedProjectId : null
      return {
        hosts: st.hosts,
        identities: st.identities,
        projects: st.projects,
        bots: st.bots,
        runs,
        turns,
        connected: st.connected,
        lastSeq: Math.max(s.lastSeq, st.daemon_seq),
        selectedBotId: selected,
        selectedProjectId: selectedProject,
      }
    })
    const sel = get().selectedBotId
    if (sel && !get().loadedBots[sel]) await get().loadMessages(sel)
    const proj = get().selectedProjectId
    if (proj && !get().loadedProjects[proj]) await get().loadGroupMessages(proj)
  },

  selectBot: (botId) => {
    set({ selectedBotId: botId, selectedProjectId: null, rightTab: 'chat', settingsBotId: null })
    if (botId && !get().loadedBots[botId]) void get().loadMessages(botId)
  },

  selectProject: (projectId) => {
    set((s) => ({
      selectedProjectId: projectId,
      rightTab: 'chat',
      settingsBotId: null,
      groupUnread: projectId ? { ...s.groupUnread, [projectId]: 0 } : s.groupUnread,
    }))
    if (projectId) void get().loadGroupMessages(projectId)
  },

  async loadGroupMessages(projectId) {
    try {
      const page = await api.fetchProjectMessages(projectId)
      set((s) => ({
        groupMessages: { ...s.groupMessages, [projectId]: page.messages },
        loadedProjects: { ...s.loadedProjects, [projectId]: true },
      }))
    } catch (e) {
      get().notify('error', `載入群組訊息失敗：${errText(e)}`)
    }
  },

  async sendGroupChat(projectId, text) {
    const crid = api.newClientRequestId()
    try {
      const res = await api.sendGroupChat(projectId, text, crid)
      // Lock each recipient's composer state right away (same as `sendPrompt`); the user
      // copies and turns arrive over the socket.
      set((s) => {
        const turns = { ...s.turns }
        for (const x of res.sent) {
          if (x.delivery === 'failed' || !x.turn_id) continue
          turns[x.bot_id] = {
            ...(turns[x.bot_id] ?? {}),
            [x.turn_id]: {
              ...(turns[x.bot_id]?.[x.turn_id] ?? {
                id: x.turn_id,
                conversation_id: '',
                run_id: s.runs[x.bot_id]?.id ?? null,
                bot_id: x.bot_id,
                origin: 'web' as const,
                status: 'in_flight' as const,
                client_request_id: `${crid}:${x.bot_id}`,
                created_at: new Date().toISOString(),
                completed_at: null,
              }),
              delivery: x.delivery,
            },
          }
        }
        return { turns }
      })
      if (res.sent.some((x) => x.delivery === 'unknown')) {
        get().notify('error', '部分訊息送達狀態未知（delivery=unknown），該 bot 需先放棄該回合才能再送。')
      }
      if (res.skipped.length > 0) {
        get().notify('info', `未送達：${res.skipped.map((x) => `@${x.bot_name}（${x.detail || x.reason}）`).join('、')}`)
      }
      return res
    } catch (e) {
      get().notify('error', errText(e))
      return null
    }
  },

  // 切分頁等於離開設定面板（面板是蓋在對話/終端上的）。
  setRightTab: (rightTab) => set({ rightTab, settingsBotId: null }),

  /** 設定面板永遠對著「目前選取的 bot」，所以開啟時順便切過去。 */
  openSettings: (botId) => {
    set({ selectedBotId: botId, selectedProjectId: null, rightTab: 'chat', settingsBotId: botId })
    if (!get().loadedBots[botId]) void get().loadMessages(botId)
  },

  closeSettings: () => set({ settingsBotId: null }),

  async loadMessages(botId) {
    try {
      const page = await api.fetchMessages(botId)
      set((s) => {
        const turns: Record<string, Turn> = {}
        for (const t of page.turns) turns[t.id] = t
        return {
          messages: { ...s.messages, [botId]: page.messages },
          turns: { ...s.turns, [botId]: turns },
          loadedBots: { ...s.loadedBots, [botId]: true },
        }
      })
    } catch (e) {
      get().notify('error', `載入訊息失敗：${errText(e)}`)
    }
  },

  async startBot(botId) {
    await guarded(set, get, `start:${botId}`, async () => {
      await api.startBot(botId)
      await get().refreshState()
    })
  },

  async stopBot(botId) {
    await guarded(set, get, `stop:${botId}`, async () => {
      await api.stopBot(botId)
      await get().refreshState()
    })
  },

  async interruptBot(botId) {
    await guarded(set, get, `intr:${botId}`, async () => {
      await api.interruptBot(botId)
    })
  },

  async sendPrompt(botId, text) {
    const crid = api.newClientRequestId()
    try {
      const res = await api.sendPrompt(botId, text, crid)
      if (res.delivery === 'unknown') {
        get().notify('error', '訊息已送出但送達狀態未知（delivery=unknown），需先放棄該回合才能再送。')
      }
      if (res.delivery === 'failed') {
        // REVIEW B10: the daemon already failed the turn (e.g. agent_blocked). Seeding a
        // local `in_flight` turn would lock the composer until `turn_updated` arrives, so
        // just reload the conversation and leave the composer usable.
        get().notify('error', '訊息未送達（delivery=failed），請確認 agent 狀態後重試。')
        void get().loadMessages(botId)
        return false
      }
      // The user message + turn arrive over the socket; only patch the turn map here so
      // the composer locks immediately even if the frame is slow.
      set((s) => ({
        turns: {
          ...s.turns,
          [botId]: {
            ...(s.turns[botId] ?? {}),
            [res.turn_id]: {
              ...(s.turns[botId]?.[res.turn_id] ?? {
                id: res.turn_id,
                conversation_id: '',
                run_id: get().runs[botId]?.id ?? null,
                bot_id: botId,
                origin: 'web' as const,
                status: 'in_flight' as const,
                client_request_id: crid,
                created_at: new Date().toISOString(),
                completed_at: null,
              }),
              delivery: res.delivery,
            },
          },
        },
      }))
      return true
    } catch (e) {
      get().notify('error', errText(e))
      return false
    }
  },

  async sendKeys(botId, keys) {
    try {
      await api.sendKeys(botId, keys, get().runs[botId]?.id ?? null)
    } catch (e) {
      get().notify('error', `送出按鍵失敗：${errText(e)}`)
    }
  },

  async abandonTurn(botId, turnId) {
    try {
      await api.abandonTurn(turnId)
      await get().loadMessages(botId)
    } catch (e) {
      get().notify('error', errText(e))
    }
  },

  async addIdentity(input) {
    try {
      await api.createIdentity(input)
      await get().refreshState()
      get().notify('info', `已新增身份 ${input.name}`)
      return true
    } catch (e) {
      get().notify('error', errText(e))
      return false
    }
  },

  async removeIdentity(name) {
    try {
      await api.deleteIdentity(name)
      await get().refreshState()
      get().notify('info', `已刪除身份 ${name}`)
    } catch (e) {
      get().notify('error', errText(e))
    }
  },

  async addHost(input) {
    try {
      const res = await api.createHost(input)
      await get().refreshState()
      if (res.connected) get().notify('info', `主機 ${res.name} 已連線`)
      else get().notify('error', `主機 ${res.name} 連線失敗：${res.error ?? '未知原因'}`)
      return res
    } catch (e) {
      get().notify('error', errText(e))
      return null
    }
  },

  async removeHost(name) {
    try {
      await api.deleteHost(name)
      await get().refreshState()
      get().notify('info', `已刪除主機 ${name}`)
    } catch (e) {
      get().notify('error', errText(e))
    }
  },

  async reconnectHost(name) {
    let res: HostResult | null = null
    await guarded(set, get, `host:${name}`, async () => {
      res = await api.reconnectHost(name)
      await get().refreshState()
      const r = res as HostResult
      if (r.connected) get().notify('info', `主機 ${r.name} 已重新連線`)
      else get().notify('error', `主機 ${r.name} 重連失敗：${r.error ?? '未知原因'}`)
    })
    return res
  },

  async addProject(input) {
    try {
      await api.createProject(input)
      await get().refreshState()
      get().notify('info', `已新增 Project ${input.label || input.path}`)
      return true
    } catch (e) {
      get().notify('error', errText(e))
      return false
    }
  },

  async addBot(projectId, input) {
    try {
      const id = await api.createBot(projectId, input)
      await get().refreshState()
      if (id) set({ selectedBotId: id })
      get().notify('info', `已新增 Bot ${input.name}`)
      return true
    } catch (e) {
      get().notify('error', errText(e))
      return false
    }
  },

  async patchBot(botId, input) {
    if (Object.keys(input).length === 0) return false
    let needsRestart: boolean | null = null
    await guarded(set, get, `patch:${botId}`, async () => {
      const res = await api.patchBot(botId, input)
      needsRestart = res.needs_restart
      await get().refreshState()
    })
    return needsRestart
  },

  async restartBot(botId) {
    let ok = false
    await guarded(set, get, `restart:${botId}`, async () => {
      await api.restartBot(botId)
      await get().refreshState()
      ok = true
    })
    return ok
  },

  async removeBot(botId) {
    // SPEC: selection moves to the next bot in the same project, else null.
    const s0 = get()
    const bot = s0.bots.find((b) => b.id === botId)
    const siblings = bot ? s0.bots.filter((b) => b.project_id === bot.project_id) : []
    const i = siblings.findIndex((b) => b.id === botId)
    const next = (siblings[i + 1] ?? siblings[i - 1] ?? null)?.id ?? null
    try {
      await api.deleteBot(botId)
      set((s) => ({
        selectedBotId: s.selectedBotId === botId ? next : s.selectedBotId,
        settingsBotId: s.settingsBotId === botId ? null : s.settingsBotId,
      }))
      await get().refreshState()
      // `refreshState` falls back to `bots[0]` when nothing is selected; honour the
      // explicit "no sibling left" case instead.
      if (next === null && get().selectedBotId !== null && !get().bots.some((b) => b.id === botId)) {
        set({ selectedBotId: null })
      }
      get().notify('info', `已刪除 Bot ${bot?.name ?? botId}`)
    } catch (e) {
      get().notify('error', errText(e))
    }
  },

  async removeProject(projectId) {
    try {
      await api.deleteProject(projectId)
      await get().refreshState()
    } catch (e) {
      get().notify('error', errText(e))
    }
  },

  readTerminal: (botId, source, lines) => api.fetchTerminal(botId, source, lines),
}))

type SetFn = (partial: Partial<StoreState> | ((s: StoreState) => Partial<StoreState>)) => void
type GetFn = () => StoreState

async function guarded(set: SetFn, get: GetFn, key: string, fn: () => Promise<void>) {
  if (get().busy[key]) return
  set((s) => ({ busy: { ...s.busy, [key]: true } }))
  try {
    await fn()
  } catch (e) {
    get().notify('error', errText(e))
  } finally {
    set((s) => {
      const busy = { ...s.busy }
      delete busy[key]
      return { busy }
    })
  }
}

// ------------------------------------------------------------------ websocket

let disconnect: (() => void) | null = null

function connectSocket(set: SetFn, get: GetFn) {
  disconnect?.()
  disconnect = api.openSocket({
    since: () => get().lastSeq,
    onStatus: (socket) => set({ socket }),
    onFrame: (frame) => handleFrame(set, get, frame),
  })
}

let resyncPending = false

function handleFrame(set: SetFn, get: GetFn, frame: { seq?: number; type: string; data?: unknown }) {
  if (typeof frame.seq === 'number') {
    set((s) => ({ lastSeq: Math.max(s.lastSeq, frame.seq as number) }))
  }
  const data = frame.data
  switch (frame.type) {
    case 'resync': {
      if (resyncPending) return
      resyncPending = true
      void (async () => {
        try {
          await get().refreshState()
          const sel = get().selectedBotId
          if (sel) await get().loadMessages(sel)
          const proj = get().selectedProjectId
          if (proj) await get().loadGroupMessages(proj)
        } finally {
          resyncPending = false
        }
      })()
      return
    }
    case 'daemon_status': {
      // SPEC §11.6: `{herdr_connected, hosts: {<name>: {connected, error?}}}`.
      // The pre-§11 shape was `{connected}`; accept both.
      if (!isRec(data)) return
      set((s) => {
        const patch: Partial<StoreState> = {
          connected: bool(pick(data, 'herdr_connected', 'connected'), s.connected),
        }
        const raw = pick(data, 'hosts')
        if (raw !== undefined) patch.hosts = mergeHosts(s.hosts, hostArray(raw))
        return patch
      })
      return
    }
    case 'host_changed': {
      if (!isRec(data)) return
      const name = str(pick(data, 'name', 'host'))
      if (!name) return
      if (name === 'local') {
        // The reserved local entry maps onto `connected`, not the hosts list.
        set({ connected: bool(pick(data, 'connected'), get().connected) })
        return
      }
      if (bool(pick(data, 'deleted'), false)) {
        set((s) => ({ hosts: s.hosts.filter((h) => h.name !== name) }))
        return
      }
      const known = get().hosts.some((h) => h.name === name)
      if (!known) {
        // Added elsewhere, or the deletion notice (`error: "removed"`, always paired with
        // `project_changed`) — either way the full record comes from `GET /api/state`.
        void get().refreshState()
        return
      }
      set((s) => ({ hosts: mergeHosts(s.hosts, [data]) }))
      return
    }
    case 'bot_status': {
      const botId = frameBotId(data)
      if (!botId) return
      const run = toRun(isRec(data) ? (data.run ?? null) : null, botId)
      set((s) => ({
        runs: { ...s.runs, [botId]: run },
        connected: isRec(data) && data.connected !== undefined ? bool(data.connected, true) : s.connected,
      }))
      return
    }
    case 'message_added': {
      const botId = frameBotId(data)
      const msg = toMessage(unwrap(data, 'message'), botId ?? undefined)
      if (!msg || !botId) {
        // Cannot attribute it — reload the visible conversation.
        const sel = get().selectedBotId
        if (sel) void get().loadMessages(sel)
        return
      }
      set((s) => {
        const patch: Partial<StoreState> = {}
        const existing = s.messages[botId] ?? []
        if (!existing.some((m) => m.id === msg.id)) {
          patch.messages = { ...s.messages, [botId]: sortByTime([...existing, msg]) }
        }
        // §13: route the same frame into its project's group timeline + unread counter.
        const bot = s.bots.find((b) => b.id === botId)
        if (bot) {
          const pid = bot.project_id
          const group = s.groupMessages[pid]
          if (group && !group.some((m) => m.id === msg.id)) {
            patch.groupMessages = { ...s.groupMessages, [pid]: sortById([...group, { ...msg, bot_id: botId, bot_name: bot.name }]) }
          }
          if (msg.role !== 'user' && s.selectedProjectId !== pid) {
            patch.groupUnread = { ...s.groupUnread, [pid]: (s.groupUnread[pid] ?? 0) + 1 }
          }
        }
        return patch
      })
      return
    }
    case 'turn_updated': {
      const botId = frameBotId(data)
      const turn = toTurn(unwrap(data, 'turn'), botId ?? undefined)
      if (!turn || !botId) return
      set((s) => ({ turns: { ...s.turns, [botId]: { ...(s.turns[botId] ?? {}), [turn.id]: turn } } }))
      return
    }
    case 'identities_changed':
    case 'project_changed':
    case 'bot_changed': {
      void get().refreshState()
      return
    }
    default:
      return
  }
}

/** Patch connection state onto the known hosts without losing their config fields. */
function mergeHosts(current: Host[], updates: unknown[]): Host[] {
  let changed = false
  const next = current.map((h) => {
    const u = updates.find((x) => isRec(x) && str(pick(x, 'name', 'host')) === h.name)
    if (!isRec(u)) return h
    const connected = bool(pick(u, 'connected', 'ok', 'up'), h.connected)
    const error = u.error !== undefined || u.last_error !== undefined
      ? optStr(pick(u, 'error', 'last_error'))
      : connected
        ? null
        : h.error
    if (connected === h.connected && error === h.error) return h
    changed = true
    return { ...h, connected, error }
  })
  return changed ? next : current
}

// ----------------------------------------------------------------- selectors

/** `"local"` (or an unknown name) means the local machine. */
export function hostOfProject(state: StoreState, projectId: string | null): Host | null {
  const project = state.projects.find((p) => p.id === projectId)
  if (!project || project.host === 'local') return null
  return state.hosts.find((h) => h.name === project.host) ?? null
}

export function hostOfBot(state: StoreState, botId: string | null): Host | null {
  const bot = state.bots.find((b) => b.id === botId)
  return bot ? hostOfProject(state, bot.project_id) : null
}

/** The host name a project sits on, even when that host is not in `hosts` (yet). */
export function projectHostName(state: StoreState, projectId: string | null): string {
  return state.projects.find((p) => p.id === projectId)?.host ?? 'local'
}

/**
 * SPEC §11.6: a bot whose host is down is `disconnected` (grey) regardless of its last
 * known run state.
 */
export function botLamp(state: StoreState, botId: string): Lamp {
  const host = hostOfBot(state, botId)
  if (host !== null) return lampOf(state.runs[botId], host.connected)
  const bot = state.bots.find((b) => b.id === botId)
  // A project pointing at a host the daemon no longer reports is treated as down.
  if (bot && projectHostName(state, bot.project_id) !== 'local') return 'disconnected'
  return lampOf(state.runs[botId], state.connected)
}

export function inFlightTurn(state: StoreState, botId: string): Turn | null {
  const run = state.runs[botId]
  const map = state.turns[botId] ?? {}
  for (const t of Object.values(map)) {
    if (t.status !== 'in_flight') continue
    if (run && t.run_id && t.run_id !== run.id) continue
    return t
  }
  return null
}

export function unknownDeliveryTurn(state: StoreState, botId: string): Turn | null {
  const map = state.turns[botId] ?? {}
  for (const t of Object.values(map)) {
    if (t.delivery === 'unknown' && t.status !== 'failed') return t
  }
  return null
}

/** SPEC §3.2 / §6.3: why the composer is locked. */
export function composerState(state: StoreState, botId: string | null): ComposerState {
  const base: ComposerState = { disabled: true, reason: '', inFlightTurnId: null, unknownTurnId: null }
  if (!botId) return { ...base, reason: '請先在左側選擇一個 Bot' }
  const bot = state.bots.find((b) => b.id === botId)
  const hostName = bot ? projectHostName(state, bot.project_id) : 'local'
  if (hostName !== 'local') {
    const host = hostOfBot(state, botId)
    if (!host || !host.connected) {
      return { ...base, reason: `主機未連線（${hostName}）${host?.error ? `：${host.error}` : ''}` }
    }
  } else if (!state.connected) {
    return { ...base, reason: 'daemon 與 herdr 的連線中斷，無法送出訊息' }
  }
  const run = state.runs[botId]
  if (!run) return { ...base, reason: 'Bot 尚未啟動，請先按「啟動」' }
  if (run.state !== 'running') return { ...base, reason: `Run 狀態為 ${run.state}，尚無法送出訊息` }
  if (run.agent_status === 'blocked') {
    return { ...base, reason: 'agent 正在等待終端回應，請在上方面板按鍵處理' }
  }
  const unknown = unknownDeliveryTurn(state, botId)
  if (unknown) {
    return { ...base, reason: '上一回合的送達狀態未知，請先「放棄該回合」或停止 Bot', unknownTurnId: unknown.id }
  }
  const inflight = inFlightTurn(state, botId)
  if (inflight) {
    return { ...base, reason: '上一則訊息仍在進行中，等待回覆或按「中斷」', inFlightTurnId: inflight.id }
  }
  return { disabled: false, reason: '', inFlightTurnId: null, unknownTurnId: null }
}

/** SPEC §13.5 composer rule: the group composer is open as long as *one* member can be sent to. */
export interface GroupComposerState {
  disabled: boolean
  reason: string
  /** member bots that would accept a prompt right now */
  sendable: string[]
}

export function groupComposerState(state: StoreState, projectId: string | null): GroupComposerState {
  if (!projectId) return { disabled: true, reason: '', sendable: [] }
  const members = state.bots.filter((b) => b.project_id === projectId)
  if (members.length === 0) return { disabled: true, reason: '這個 Project 還沒有 Bot', sendable: [] }
  const sendable = members.filter((b) => !composerState(state, b.id).disabled).map((b) => b.id)
  if (sendable.length === 0) {
    const first = composerState(state, members[0].id).reason
    return { disabled: true, reason: `專案內沒有可送訊息的 Bot（${first}）`, sendable }
  }
  return { disabled: false, reason: '', sendable }
}
