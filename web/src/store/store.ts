/**
 * Single Zustand store: server state mirrored from `GET /api/state` + `/ws`, plus UI state.
 *
 * Event flow (SPEC §7.3): the socket carries `{seq, type, data}` frames. We track the highest
 * seq and hand it back as `?since=` on reconnect; a `resync` frame (or a socket that reopens
 * with a gap) triggers a full `GET /api/state` plus a message reload for the selected bot.
 */

import { create } from 'zustand'
import * as api from '../api'
import { frameBotId, lampOf, sortByTime, toMessage, toRun, toTurn, unwrap, isRec, bool, pick } from '../api/normalize'
import { ApiError } from '../api/types'
import type { Bot, Lamp, Message, NewBotInput, NewProjectInput, Project, Run, TerminalSource, Turn } from '../api/types'

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
  /** daemon <-> herdr link (SPEC §2.2 "連線") */
  connected: boolean
  lastSeq: number

  projects: Project[]
  bots: Bot[]
  runs: Record<string, Run | null>
  turns: Record<string, Record<string, Turn>>
  messages: Record<string, Message[]>
  loadedBots: Record<string, boolean>

  selectedBotId: string | null
  rightTab: RightTab
  notices: Notice[]
  busy: Record<string, boolean>

  bootstrap: () => Promise<void>
  refreshState: () => Promise<void>
  selectBot: (botId: string | null) => void
  setRightTab: (tab: RightTab) => void
  loadMessages: (botId: string) => Promise<void>
  notify: (kind: Notice['kind'], text: string) => void
  dismiss: (id: number) => void

  startBot: (botId: string) => Promise<void>
  stopBot: (botId: string) => Promise<void>
  interruptBot: (botId: string) => Promise<void>
  sendPrompt: (botId: string, text: string) => Promise<boolean>
  sendKeys: (botId: string, keys: string[]) => Promise<void>
  abandonTurn: (botId: string, turnId: string) => Promise<void>
  addProject: (input: NewProjectInput) => Promise<boolean>
  addBot: (projectId: string, input: NewBotInput) => Promise<boolean>
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

  projects: [],
  bots: [],
  runs: {},
  turns: {},
  messages: {},
  loadedBots: {},

  selectedBotId: null,
  rightTab: 'chat',
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
      return {
        projects: st.projects,
        bots: st.bots,
        runs,
        turns,
        connected: st.connected,
        lastSeq: Math.max(s.lastSeq, st.daemon_seq),
        selectedBotId: selected,
      }
    })
    const sel = get().selectedBotId
    if (sel && !get().loadedBots[sel]) await get().loadMessages(sel)
  },

  selectBot: (botId) => {
    set({ selectedBotId: botId, rightTab: 'chat' })
    if (botId && !get().loadedBots[botId]) void get().loadMessages(botId)
  },

  setRightTab: (rightTab) => set({ rightTab }),

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

  async removeBot(botId) {
    try {
      await api.deleteBot(botId)
      set((s) => ({ selectedBotId: s.selectedBotId === botId ? null : s.selectedBotId }))
      await get().refreshState()
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
        } finally {
          resyncPending = false
        }
      })()
      return
    }
    case 'daemon_status': {
      if (isRec(data)) set({ connected: bool(pick(data, 'connected'), true) })
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
        const existing = s.messages[botId] ?? []
        if (existing.some((m) => m.id === msg.id)) return {}
        return { messages: { ...s.messages, [botId]: sortByTime([...existing, msg]) } }
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
    case 'project_changed':
    case 'bot_changed': {
      void get().refreshState()
      return
    }
    default:
      return
  }
}

// ----------------------------------------------------------------- selectors

export function botLamp(state: StoreState, botId: string): Lamp {
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
  if (!state.connected) return { ...base, reason: 'daemon 與 herdr 的連線中斷，無法送出訊息' }
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
