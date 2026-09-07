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
  toKindQuota,
  toMemSnapshot,
  toIdentityStatusMap,
  toToolMap,
  optStr,
  sortById,
  sortByTime,
  str,
  toMessage,
  toRun,
  toTeam,
  toTeamTask,
  toTeamEvent,
  toTurn,
  unwrap,
  isRec,
  bool,
  pick,
} from '../api/normalize'
import { ApiError } from '../api/types'
import type { Bot, BotKind, GroupChatResult, MemSnapshot, GroupMessage, Host, HostResult, HostShell, Identity, IdentityStatusMap, Lamp, Message, ModelInfo, NewBotInput, NewHostInput, NewIdentityInput, NewProjectInput, NewTeamInput, PatchBotInput, PatchProjectInput, PatchTeamInput, Project, QuotaMap, Run, Team, TeamBranchDisposal, TeamControlAction, TeamDetail, TeamEvent, TeamTaskDecision, TerminalSource, ToolMap, Turn } from '../api/types'
import { dropHostModels, modelsKey, shouldFetchModels, type ModelsCache } from './modelsCache'
import { MESSAGE_CAP, TEAM_EVENT_CAP, byId, byTime, capList, insertSorted, pruneTurns } from './lists'
import { acceptStateSeq, singleFlight } from './singleFlight'
import { botStatusConnTarget } from './botStatusConn'
import {
  botKey,
  completesTurn,
  completionKey,
  countUnreadTurns,
  groupKey,
  loadCounts,
  loadMarks,
  markNow,
  markOfMessages,
  pruneMarks,
  pruneUnread,
  saveCounts,
  saveMarks,
  takeTurnCompletion,
  windowActive,
  type ReadMark,
} from './unread'
import { BOT_KINDS, LOCAL_HOST, quotaKey, TEAM_PHASE_LABEL, TEAM_TERMINAL_PHASES } from '../api/types'

export type SocketStatus = 'connecting' | 'open' | 'closed'
export type RightTab = 'chat' | 'terminal'

/** 觸發按鈕的 viewport 矩形（只留彈窗定位需要的四邊）。 */
export type SettingsAnchor = { left: number; right: number; top: number; bottom: number }

/** 從觸發元素取 anchor：DOMRect 直接存進 store 會帶著一堆用不到的欄位。 */
export function anchorOf(el: Element): SettingsAnchor {
  const r = el.getBoundingClientRect()
  return { left: r.left, right: r.right, top: r.top, bottom: r.bottom }
}
/** v4.0 global preference: how a bot's kind is shown (icon glyph or the word). */
export type KindDisplay = 'icon' | 'text'

const KIND_DISPLAY_KEY = 'am.kindDisplay'
const BOT_ORDER_KEY = 'am.botOrder'
const PROJECT_ORDER_KEY = 'am.projectOrder'
const DRAFTS_KEY = 'am.drafts'
const DRAFT_CURSORS_KEY = 'am.draftCursors'
const SELECTION_KEY = 'am.selection'

/**
 * Which conversation is open, mirrored to localStorage so a reload lands on the same one.
 * Both halves matter: `projectId` non-null means the right pane shows that project's group
 * chat (SPEC §13) while `botId` stays parked for when the user switches back.
 */
interface Selection {
  botId: string | null
  projectId: string | null
  /** SPEC-team §11.5：與另外兩個互斥；非 null = 右側顯示 TeamPanel。 */
  teamId: string | null
}

const NO_SELECTION: Selection = { botId: null, projectId: null, teamId: null }

function readSelection(): Selection {
  try {
    const raw = localStorage.getItem(SELECTION_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!isRec(parsed)) return NO_SELECTION
    return {
      botId: optStr(pick(parsed, 'botId')),
      projectId: optStr(pick(parsed, 'projectId')),
      teamId: optStr(pick(parsed, 'teamId')),
    }
  } catch {
    return NO_SELECTION
  }
}

function writeSelection(sel: Selection) {
  try {
    localStorage.setItem(SELECTION_KEY, JSON.stringify(sel))
  } catch {
    /* storage unavailable: the selection still holds for this page */
  }
}

/** Read once at module load so the store's initial state is already the restored selection. */
const initialSelection = readSelection()

const SHELL_VIEW_KEY = 'am.shellView'

type ShellView = { host: string; paneId: string; cwd: string }

/**
 * 開著的主機 shell 也鏡射到 localStorage：重新整理要回到同一個 shell，不是回到對話。
 * pane 在這段時間可能已經被關掉，所以 `bootstrap` 會拿 `GET /api/hosts/:host/shells` 對一次，
 * 不在了就清掉。
 */
function readShellView(): ShellView | null {
  try {
    const raw = localStorage.getItem(SHELL_VIEW_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!isRec(parsed)) return null
    const host = optStr(pick(parsed, 'host'))
    const paneId = optStr(pick(parsed, 'paneId'))
    if (!host || !paneId) return null
    return { host, paneId, cwd: optStr(pick(parsed, 'cwd')) ?? '' }
  } catch {
    return null
  }
}

function writeShellView(v: ShellView | null) {
  try {
    if (v) localStorage.setItem(SHELL_VIEW_KEY, JSON.stringify(v))
    else localStorage.removeItem(SHELL_VIEW_KEY)
  } catch {
    /* storage unavailable */
  }
}

const initialShellView = readShellView()

/**
 * Composer drafts survive bot / group / team / tab switches and reloads.
 * Key: `bot:<id>` | `group:<projectId>` | `team:<teamId>`.
 */
export type DraftKey = `bot:${string}` | `group:${string}` | `team:${string}`

/** The saved selection in a composer draft (usually a collapsed caret). */
export interface DraftCursor {
  start: number
  end: number
}

function readDrafts(): Record<string, string> {
  try {
    const raw = localStorage.getItem(DRAFTS_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!isRec(parsed)) return {}
    const out: Record<string, string> = {}
    for (const [k, v] of Object.entries(parsed)) if (typeof v === 'string' && v) out[k] = v
    return out
  } catch {
    return {}
  }
}

function writeDrafts(drafts: Record<string, string>) {
  try {
    localStorage.setItem(DRAFTS_KEY, JSON.stringify(drafts))
  } catch {
    /* storage unavailable: drafts still live for this page */
  }
}

function readDraftCursors(): Record<string, DraftCursor> {
  try {
    const raw = localStorage.getItem(DRAFT_CURSORS_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!isRec(parsed)) return {}
    const out: Record<string, DraftCursor> = {}
    for (const [k, v] of Object.entries(parsed)) {
      if (!isRec(v)) continue
      const start = v.start
      const end = v.end
      if (typeof start !== 'number' || !Number.isFinite(start) || start < 0) continue
      if (typeof end !== 'number' || !Number.isFinite(end) || end < 0) continue
      out[k] = { start: Math.floor(start), end: Math.floor(end) }
    }
    return out
  } catch {
    return {}
  }
}

function writeDraftCursors(cursors: Record<string, DraftCursor>) {
  try {
    localStorage.setItem(DRAFT_CURSORS_KEY, JSON.stringify(cursors))
  } catch {
    /* storage unavailable: cursors still live for this page */
  }
}

/**
 * 側欄裡每個 project 的 bot 順序（bot id 陣列）。daemon 沒有排序欄位，所以這是純前端偏好，
 * 存在 localStorage；沒被列到的 bot（新增的）沿用 daemon 回來的順序接在後面。
 */
function readBotOrder(): Record<string, string[]> {
  try {
    const raw = localStorage.getItem(BOT_ORDER_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!isRec(parsed)) return {}
    const out: Record<string, string[]> = {}
    for (const [k, v] of Object.entries(parsed)) {
      if (Array.isArray(v)) out[k] = v.filter((x): x is string => typeof x === 'string')
    }
    return out
  } catch {
    return {}
  }
}

function readProjectOrder(): string[] {
  try {
    const raw = localStorage.getItem(PROJECT_ORDER_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    return Array.isArray(parsed) ? parsed.filter((x): x is string => typeof x === 'string') : []
  } catch {
    return []
  }
}

function writeProjectOrder(order: string[]) {
  try {
    localStorage.setItem(PROJECT_ORDER_KEY, JSON.stringify(order))
  } catch {
    /* storage unavailable: the order still holds for this page */
  }
}

function writeBotOrder(order: Record<string, string[]>) {
  try {
    localStorage.setItem(BOT_ORDER_KEY, JSON.stringify(order))
  } catch {
    /* storage unavailable: the order still holds for this page */
  }
}

function readKindDisplay(): KindDisplay {
  try {
    return localStorage.getItem(KIND_DISPLAY_KEY) === 'text' ? 'text' : 'icon'
  } catch {
    return 'icon'
  }
}

/**
 * 未讀（見 `store/unread.ts`）：帳本存在 localStorage，store 只掛最小的 hook。
 * 已讀標記留在模組層而不進 state——沒有任何畫面直接讀它，進 state 只會多一次 render。
 */
const initialUnread = loadCounts()
let readMarks = loadMarks()

/** 記下某個對話讀到哪裡。`key` 是 `bot:<id>` 或 `group:<projectId>`。 */
function setReadMark(key: string, mark: ReadMark) {
  readMarks = { ...readMarks, [key]: mark }
  saveMarks(readMarks)
}

function persistUnread(s: { botUnread: Record<string, number>; groupUnread: Record<string, number> }) {
  saveCounts({ bots: s.botUnread, groups: s.groupUnread })
}

export interface Notice {
  id: number
  kind: 'error' | 'info'
  text: string
  /**
   * 一個可以按的動作（目前只有刪除 bot 之後的「復原」）。撤銷放在通知上而不是另開一個
   * 面板：誤刪的當下人就在看那則通知，多開一層反而更遠。
   */
  action?: { label: string; run: () => void | Promise<void> }
}

/** WS `turn_progress` (API.md v3.9): the partial reply of an in-flight turn. */
/** `turn_progress` 合併間隔（毫秒）：同一個 bot 在這段時間內只套用最後一個 frame。 */
const LIVE_THROTTLE_MS = 250
const liveThrottle = new Map<string, { apply: null | (() => void) }>()

/** issue #25：「載入更早的訊息」一次補幾則（與第一頁同大小）。 */
const PAGE_SIZE = 200

export interface LiveReply {
  turnId: string
  text: string
  /**
   * What the agent is doing right now (`turn_progress.activity`, API.md v4.1): the pane's
   * spinner row — `Thinking… (12s · ↑ 1.2k tokens)`, a tool name. Plain terminal text, never
   * Markdown, and never part of the stored message; shown only while `text` is still empty.
   * `''` when the frame carried none.
   */
  activity: string
  /**
   * A retry / API-error banner on the pane (`turn_progress.alert`, API.md v4.2) —
   * `API error · Retrying in 3s · attempt 1/10`. The turn is still in flight and the spinner
   * still spins, so without this the UI looks healthy while the agent is stuck retrying an
   * upstream failure. Plain terminal text. `''` when the frame carried none.
   */
  alert: string
  revision: number
}

/** 排隊中的一則送出（`queuedSends`）。 */
export interface QueuedSend {
  text: string
  attachments: string[]
}

export interface ComposerState {
  /** 完全不能輸入（未啟動、blocked、主機斷線、送達狀態未知…）。 */
  disabled: boolean
  reason: string
  /**
   * 這一回合還在跑：可以照常打字，按送出會排進佇列，等回合結束自動送出
   * （daemon 一個 run 同時只允許一個 in-flight turn，直接送會 409）。
   */
  queued: boolean
  inFlightTurnId: string | null
  unknownTurnId: string | null
}

export interface StoreState {
  ready: boolean
  bootError: string | null
  socket: SocketStatus
  /** daemon <-> 本機 herdr link (SPEC §2.2 "連線") */
  connected: boolean
  /** The observed user's Herdr default session, used by imported default-session bots. */
  defaultConnected: boolean
  lastSeq: number

  /** SPEC §11.6 remote hosts; the local machine is never in this list. */
  hosts: Host[]
  /** v4.0: `herdr --session …` for the local machine (remote ones carry their own). */
  attachCommand: string
  /** v4.0: agent CLI detection on the local machine (`hosts[0].tools`). */
  localTools: ToolMap
  /** v4.0: per-identity login detection on the local machine (`hosts[0].identities`). */
  localIdentityStatus: IdentityStatusMap
  /** SPEC §15: `GET /api/mem` + WS `mem_updated`；null = 舊 daemon 沒有這支。 */
  mem: MemSnapshot | null
  /** v4.0: `GET /api/quota` + WS `quota_updated`; key = kind or `kind:identity`. */
  quota: QuotaMap
  /** v4.0: `GET /api/models` cache, keyed `kind@host@identity`; null = fetch failed (use static list). */
  models: ModelsCache
  /** issue #26: when a `models[key]` fetch last failed; retried after `MODELS_RETRY_MS`. */
  modelsFailedAt: Record<string, number>
  /** Forget every cached model list for `host` (the host came back / tools were re-detected). */
  dropHostModels: (host: string) => void
  kindDisplay: KindDisplay
  /** The "host is missing <kind>" banner, closed for this page load. */
  toolHintDismissed: boolean
  /** v4.0: unsent composer text per bot / group (mirrored to localStorage). */
  drafts: Record<string, string>
  /** The last selection/caret for each unsent composer draft (mirrored to localStorage). */
  draftCursors: Record<string, DraftCursor>
  identities: Identity[]
  projects: Project[]
  bots: Bot[]
  runs: Record<string, Run | null>
  turns: Record<string, Record<string, Turn>>
  messages: Record<string, Message[]>
  loadedBots: Record<string, boolean>
  /**
   * issue #25：`messages[botId]` / `groupMessages[projectId]` 前面還有更早的沒載進來
   * ——第一頁的 `has_more`，或後來被 `MESSAGE_CAP` 截掉的。true 時對話最上方出現
   * 「載入更早的訊息」，按下去走 `before=` 分頁。key 是 bot id 或 project id。
   */
  moreMessages: Record<string, boolean>
  /** 上面那顆按鈕正在抓（同一個 key 不重複發）。 */
  loadingMore: Record<string, boolean>
  /**
   * bot id → 已完成但使用者還沒看到的回合數（進行中 → 已完成（未讀）→ 已完成（已讀）的中間段）。
   * 純前端的帳，daemon 不知情；跨重整保留在 localStorage（`store/unread.ts`）。
   */
  botUnread: Record<string, number>
  /**
   * bot_id → partial reply of its in-flight turn (`turn_progress`). Cleared when that turn's
   * assistant `message_added` arrives or `turn_updated` leaves `in_flight`. Render it only
   * while `composerState(...).inFlightTurnId === liveReply.turnId` so a stale entry never shows.
   */
  liveReply: Record<string, LiveReply>

  /**
   * 在回合進行中按下送出的訊息（每個 bot 最多一則）。回合一結束就自動送出；
   * 使用者也可以在送出前取消或改寫。
   */
  queuedSends: Record<string, QueuedSend>

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

  // ---- SPEC-team §11.5 -------------------------------------------------
  /** `GET /api/state` 的 `projects[].teams[]` 攤平；key = team id。 */
  teams: Record<string, Team>
  /** `GET /teams/:id`（含 tasks / worktree_root）；只在開過的 team 上有值。 */
  teamDetail: Record<string, TeamDetail>
  /** `GET /teams/:id/events` + WS `team_event`。 */
  teamEvents: Record<string, TeamEvent[]>
  /** 該 team 的視圖沒開著時進來的成員回覆數（僅記憶體）。 */
  teamUnread: Record<string, number>
  /** done team 清理後追加 issue 會固定得到 409；記住它以隱藏不可再用的按鈕。 */
  teamReopenUnavailable: Record<string, boolean>
  /** 非 null = 右側顯示 TeamPanel（與 `selectedProjectId` 互斥）。 */
  selectedTeamId: string | null
  /**
   * false = 這個 daemon 沒有 `/api/teams` 端點（舊版）。第一次收到 404/405 就翻成 false，
   * 之後 UI 的 team 入口靜默消失，不再重試，也不再跳錯誤（docs/FRONTEND.md §8）。
   */
  teamsSupported: boolean
  /** TeamLaunchPanel（右側暫時性 sheet）；null = 未開啟。 */
  teamLaunch: { projectId: string; issueNumber: number; repo: string } | null

  /**
   * 非 null = 主面板顯示 `HostShellPanel`（與上面每一個選取互斥，而且優先）。
   *
   * 刻意不寫進 localStorage 的選取記憶：`paneId` 活不過 daemon 重啟，記住它只會在下次
   * 開啟時指向一個已經不存在的 shell。
   */
  shellView: { host: string; paneId: string; cwd: string } | null
  /**
   * false = 這版 daemon 沒有 `/api/hosts/:name/shells`。第一次撞到就翻成 false，
   * 「開 shell」的入口從此靜默消失（docs/FRONTEND.md §8）。
   */
  hostShellSupported: boolean

  selectedBotId: string | null
  rightTab: RightTab
  /** 開著「Bot 設定」面板的 bot id（null = 面板關閉）。 */
  settingsBotId: string | null
  /** 觸發設定的按鈕位置（viewport 座標），彈窗會貼著它開；null = 置中。 */
  settingsAnchor: SettingsAnchor | null
  /** 使用者拖出來的 bot 順序，key = project id（見 `botsOfProject`）。 */
  botOrder: Record<string, string[]>
  /** 側欄專案的拖曳順序（project id），同 `botOrder` 只存瀏覽器；沒列到的接在後面。 */
  projectOrder: string[]
  /** Sidebar consumes this to open the「新增 Bot」sheet for a project. */
  openBotSheetFor: string | null
  notices: Notice[]
  busy: Record<string, boolean>

  bootstrap: () => Promise<void>
  refreshState: () => Promise<void>
  selectBot: (botId: string | null) => void
  /** ↑/↓ 換 bot：以側邊欄看到的順序往前 / 後選一個（頭尾繞回去）。 */
  selectAdjacentBot: (dir: -1 | 1) => void
  /** Open the §13 group view of a project (null = back to the selected bot). */
  selectProject: (projectId: string | null) => void
  loadGroupMessages: (projectId: string) => Promise<void>
  /** issue #25：往前補一頁群組時間軸（`before=` 目前最舊的一則）。 */
  loadEarlierGroupMessages: (projectId: string) => Promise<void>
  /** `POST /projects/:id/chat`; null = failed (reason already shown as a notice). */
  sendGroupChat: (projectId: string, text: string, attachments?: string[]) => Promise<GroupChatResult | null>
  setRightTab: (tab: RightTab) => void
  openSettings: (botId: string, anchor?: SettingsAnchor | null) => void
  /** 把 `botId` 移到 `beforeId` 之前（`beforeId = null` = 移到最後）。同專案內才有效。 */
  moveBot: (botId: string, beforeId: string | null) => void
  /** 把專案移到 `beforeId` 之前；null = 移到最後。 */
  moveProject: (projectId: string, beforeId: string | null) => void
  closeSettings: () => void
  /** Ask the Sidebar to open its「新增 Bot」sheet for this project. */
  requestOpenBotSheet: (projectId: string) => void
  clearOpenBotSheet: () => void
  loadMessages: (botId: string) => Promise<void>
  /** issue #25：往前補一頁對話（`before=` 目前最舊的一則）。 */
  loadEarlierMessages: (botId: string) => Promise<void>
  /** 把這個 bot 的對話標成已讀（徽章清掉、已讀標記推到最後一則）。 */
  markBotRead: (botId: string) => void
  /** 把這個 project 的群組聊天標成已讀。 */
  markGroupRead: (projectId: string) => void
  /** 視窗回到前景時：現在開著的那個對話就是被看到的那個。 */
  markCurrentRead: () => void
  /** 訊息載進來之後用已讀標記重算未讀數——存下來的數字只是重整前的快照。 */
  recountBot: (botId: string) => void
  /** 丟掉已經不存在的 bot / project 的未讀帳（每次 `GET /api/state` 之後）。 */
  pruneUnread: () => void
  notify: (kind: Notice['kind'], text: string, action?: Notice['action']) => void
  dismiss: (id: number) => void

  startBot: (botId: string) => Promise<void>
  stopBot: (botId: string) => Promise<void>
  interruptBot: (botId: string) => Promise<void>
  /** 強制結束目前回合（送不送得出 `esc` 都解鎖）。 */
  abortBot: (botId: string) => Promise<void>
  /**
   * 對正在跑的 bot 送 `/login`，讓它的 TUI 進入登入 / 切換帳號流程。
   * `true` = 已經送進去（agent 現在停在登入畫面）；`false` = 沒送出，原因已經跳通知。
   */
  loginBot: (botId: string) => Promise<boolean>
  /** `attachments` 是 `POST /bots/:id/attachments` 回傳的 id（拖放進來的圖片）。 */
  sendPrompt: (botId: string, text: string, attachments?: string[]) => Promise<boolean>
  sendKeys: (botId: string, keys: string[]) => Promise<void>
  /**
   * 把一整段（可能多行的）文字打進 bot 的 pane，`enter` 決定要不要順手送出。
   * 多行內容要走這裡：`sendKeys` 吃的是鍵名，`\n` 不是鍵名（見 `store/alongside.ts`）。
   */
  sendText: (botId: string, text: string, enter: boolean) => Promise<boolean>
  abandonTurn: (botId: string, turnId: string) => Promise<void>
  addHost: (input: NewHostInput) => Promise<HostResult | null>
  addIdentity: (input: NewIdentityInput) => Promise<boolean>
  removeIdentity: (name: string) => Promise<void>
  removeHost: (name: string) => Promise<void>
  reconnectHost: (name: string) => Promise<HostResult | null>
  addProject: (input: NewProjectInput) => Promise<boolean>
  addBot: (projectId: string, input: NewBotInput) => Promise<string | null>
  /** 「開同類分身」：同專案、同 kind/模型/身份/人設，名字自動加序號。 */
  cloneBot: (botId: string) => Promise<string | null>
  /** `PATCH /api/bots/:id` — 回傳 `needs_restart`，失敗回 null（原因已跳通知）。 */
  patchBot: (botId: string, input: PatchBotInput) => Promise<boolean | null>
  /** `PATCH /api/projects/:id` — 目前只有 label（改名）。true = 已套用。 */
  patchProject: (projectId: string, input: PatchProjectInput) => Promise<boolean>
  restartBot: (botId: string) => Promise<boolean>
  removeBot: (botId: string) => Promise<void>
  removeProject: (projectId: string) => Promise<void>
  readTerminal: (botId: string, source: TerminalSource, lines: number) => ReturnType<typeof api.fetchTerminal>

  /**
   * 在某台主機開一個 shell 並切到 `HostShellPanel`。已經有活著的就接回**最新那個**，
   * 不再多開：按第二次「開 shell」想看的是剛剛那個，不是一個空白的新終端。
   * false = 沒開起來（原因已經跳通知，或這版 daemon 沒有這個功能）。
   */
  openHostShell: (host: string, cwd?: string) => Promise<boolean>
  /** 切到一個**已經開著**的 shell（環境設定裡的清單點回去用），不打 API、不新開。 */
  viewHostShell: (shell: HostShell) => void
  /** 只關掉面板，shell 留著（回來還接得回去）。 */
  closeShellView: () => void
  /** 開機時對 localStorage 還原的 `shellView` 驗一次 pane 還在不在（不在就清掉）。 */
  restoreShellView: () => Promise<void>
  /** 真的結束這個 shell（`DELETE …/shells/:pane_id`）並關掉面板。 */
  endHostShell: (host: string, paneId: string) => Promise<void>

  // v4.0
  loadQuota: () => Promise<void>
  loadMem: () => Promise<void>
  loadModels: (kind: BotKind, host: string, identity?: string | null) => Promise<ModelInfo[] | null>
  /** Ask a running bot on `host` to install + log in `kind`; opens that bot's chat. null = failed. */
  installTool: (host: string, kind: BotKind, viaBotId: string) => Promise<string | null>
  /** v4.0: re-run CLI + per-identity login detection on one host (`''` / `local` = this machine). */
  refreshTools: (host: string) => Promise<boolean>
  setKindDisplay: (mode: KindDisplay) => void
  dismissToolHint: () => void
  /** Empty text removes the draft. */
  setDraft: (key: DraftKey, text: string) => void
  /** Save a draft's selection; positions are clamped to the current draft text. */
  setDraftCursor: (key: DraftKey, start: number, end?: number) => void

  /** 回合進行中按送出：排隊，等這回合結束再送。 */
  queueSend: (botId: string, text: string, attachments: string[]) => void
  /** 取消排隊中的送出（訊息會退回輸入框，由呼叫端決定）。 */
  cancelQueuedSend: (botId: string) => void

  // ---- SPEC-team -------------------------------------------------------
  /** 開啟某個 team 的視圖（null = 回到原本的 bot / 群組）。 */
  selectTeam: (teamId: string | null) => void
  /** 載入 `GET /teams/:id` + `/events` + 該專案的合併時間軸。 */
  loadTeam: (teamId: string) => Promise<void>
  /** IssuesBar 的「組隊」：開啟 TeamLaunchPanel。 */
  openTeamLaunch: (projectId: string, issueNumber: number, repo?: string) => void
  closeTeamLaunch: () => void
  /** `POST /projects/:id/teams`；成功後自動 `selectTeam`。null = 失敗。 */
  createTeam: (projectId: string, input: NewTeamInput) => Promise<string | null>
  controlTeam: (teamId: string, action: TeamControlAction) => Promise<boolean>
  /**
   * `DELETE /teams/:id?branches=`（SPEC-team §6.5a）——任何 phase 都可以刪。
   * `branches: 'delete'` 會連分支一起 `git branch -D`，UI 必須先二次確認。
   * team 已經不在（404 not_found）也算成功：本地照樣清乾淨。
   */
  removeTeam: (teamId: string, branches?: TeamBranchDisposal) => Promise<boolean>
  patchTeam: (teamId: string, input: PatchTeamInput) => Promise<boolean>
  /**
   * `POST /teams/:id/close-issue`（SPEC-team §10.7）——把 team 對應的 GitHub issue 關掉。
   *
   * 只有 `phase === 'done'` 的 team 有這個動作，而且**永遠是使用者按出來的**：daemon 不會
   * 自己關 issue，UI 也要先二次確認（這是會寫到 GitHub 的動作）。
   */
  closeTeamIssue: (teamId: string, issueId?: string) => Promise<boolean>
  /** `POST /teams/:id/say`（`to` = `pm` 或 bot_id）。 */
  sayToTeam: (teamId: string, text: string, to: string) => Promise<boolean>
  /** `POST /teams/:id/answer` — 回覆 PM 的 `ask_user`。 */
  answerTeam: (teamId: string, text: string) => Promise<boolean>
  decideTeamTask: (teamId: string, taskId: string, action: TeamTaskDecision, note?: string) => Promise<boolean>
  /** `POST /teams/:id/issues`（SPEC-team §2.3）——執行中續加 issue 到佇列。 */
  addTeamIssues: (teamId: string, issueNumbers: number[]) => Promise<boolean>
  /** `DELETE /teams/:id/issues/:issue_id` — 只能移除還沒開始的。 */
  removeTeamIssue: (teamId: string, issueId: string) => Promise<boolean>
}

let noticeSeq = 0

function errText(e: unknown): string {
  if (e instanceof ApiError) return `${e.message}（HTTP ${e.status}）`
  if (e instanceof Error) return e.message
  return String(e)
}

/**
 * `POST /bots/:id/login` 送不出去的理由，翻成使用者看得懂的一句話。
 *
 * daemon 那邊回的是穩定的機器 key（`login_unsupported` / `not_running` / `agent_busy` /
 * `turn_in_flight` / `no_pane`），文案留在這裡：使用者是明確按了「登入」，該知道現在為什麼
 * 不行、以及要先做什麼。認不出來的就退回一般錯誤字串。
 */
function loginErrText(e: unknown): string {
  if (!(e instanceof ApiError)) return errText(e)
  const kind = typeof e.body.kind === 'string' ? e.body.kind : '這個 agent'
  switch (e.body.error === 'login_unsupported' ? 'login_unsupported' : e.body.reason) {
    case 'login_unsupported':
      return `${kind} 的 TUI 沒有登入指令，只能到它跑的那台主機上執行 \`${kind} login\`。`
    case 'not_running':
      return '這個 bot 沒在跑。先啟動它，再按登入。'
    case 'agent_busy':
      return 'agent 正在忙，這時候打字會被吃掉。等它停下來再按。'
    case 'turn_in_flight':
      return '有回合還在進行中，登入指令會被當成那個提問的一部分。等這回合結束再按。'
    case 'no_pane':
      return '找不到這個 bot 的終端機畫面，可能已經被關掉了。重啟這個 bot 再試。'
    default:
      return errText(e)
  }
}

/**
 * 一頁 `GET .../messages` 回來時，要從舊的清單裡留下哪些訊息。
 *
 * 不能整包換掉：這個請求飛在半路的時候，socket 可能已經送來 `message_added`（切 bot 的當下
 * 正好有回覆進來，就會被舊的那一頁蓋掉）。但也不能全留——resync 的責任正是把過期狀態改對，
 * 只加不刪就修不掉了。
 *
 * 界線是「這一頁最新的一筆」：比它舊又不在頁裡的，daemon 那邊已經沒有了；比它新的，只可能是
 * 請求送出之後才進來的。頁是空的就退回請求送出的時刻（`startedAt`）。
 */
function keptAfterPage<T extends { id: string; created_at: string }>(existing: T[], page: T[], startedAt: string): T[] {
  let newest = ''
  for (const m of page) if (m.created_at > newest) newest = m.created_at
  const cutoff = newest || startedAt
  const seen = new Set(page.map((m) => m.id))
  return existing.filter((m) => !seen.has(m.id) && m.created_at > cutoff)
}

/** issue #23：最近一次套用到 store 的 `GET /api/state` 的 `daemon_seq`；更舊的快照不套用。 */
let appliedStateSeq = 0

export const useStore = create<StoreState>((set, get) => ({
  ready: false,
  bootError: null,
  socket: 'connecting',
  connected: true,
  defaultConnected: false,
  lastSeq: 0,

  hosts: [],
  attachCommand: 'herdr --session agents-manager',
  localTools: toToolMap(undefined),
  localIdentityStatus: {},
  quota: {},
  mem: null,
  models: {},
  modelsFailedAt: {},
  kindDisplay: readKindDisplay(),
  toolHintDismissed: false,
  drafts: readDrafts(),
  draftCursors: readDraftCursors(),
  identities: [],
  projects: [],
  bots: [],
  runs: {},
  turns: {},
  messages: {},
  loadedBots: {},
  moreMessages: {},
  loadingMore: {},
  botUnread: initialUnread.bots,
  liveReply: {},
  queuedSends: {},

  selectedProjectId: initialSelection.projectId,
  groupMessages: {},
  loadedProjects: {},
  groupUnread: initialUnread.groups,

  teams: {},
  teamDetail: {},
  teamEvents: {},
  teamUnread: {},
  teamReopenUnavailable: {},
  selectedTeamId: initialSelection.teamId,
  teamsSupported: true,
  teamLaunch: null,
  shellView: initialShellView,
  hostShellSupported: true,

  selectedBotId: initialSelection.botId,
  rightTab: 'chat',
  settingsBotId: null,
  settingsAnchor: null,
  botOrder: readBotOrder(),
  projectOrder: readProjectOrder(),
  openBotSheetFor: null,
  notices: [],
  busy: {},

  notify: (kind, text, action) => {
    noticeSeq += 1
    const id = noticeSeq
    set((s) => ({ notices: [...s.notices, { id, kind, text, action }] }))
    // 帶動作的通知留久一點：4 秒不夠一個人意識到自己按錯了。
    setTimeout(() => get().dismiss(id), action ? 15000 : kind === 'error' ? 8000 : 4000)
  },

  dismiss: (id) => set((s) => ({ notices: s.notices.filter((n) => n.id !== id) })),

  async bootstrap() {
    try {
      await api.session()
      await get().refreshState()
      // 開機時還原的 team 選取要在這裡補抓細節（`refreshState` 不再幫忙載 team，issue #23）。
      const team = get().selectedTeamId
      if (team) await get().loadTeam(team)
      await get().restoreShellView()
      set({ ready: true, bootError: null })
    } catch (e) {
      set({ ready: false, bootError: errText(e) })
      return
    }
    connectSocket(set, get)
    void get().loadQuota()
    void get().loadMem()
  },

  // issue #23：single-flight + trailing——一次操作連發 N 個 frame 只換來一、兩次 `GET /api/state`，
  // 回應也因此天生有序。`acceptStateSeq` 是額外保險，比已套用過的 `daemon_seq` 舊的快照直接丟。
  refreshState: singleFlight(async () => {
    const st = await api.fetchState()
    const seq = acceptStateSeq(appliedStateSeq, st.daemon_seq)
    if (seq === null) return
    // A daemon that has just come back can answer with an empty snapshot for a moment. While
    // the socket is not open that is "not yet", not "everything was deleted": keep what we
    // have rather than blanking the sidebar into the "no projects" onboarding.
    if (st.projects.length === 0 && get().projects.length > 0 && get().socket !== 'open') return
    appliedStateSeq = seq
    const runs: Record<string, Run | null> = {}
    for (const b of st.bots) runs[b.id] = st.runs.find((r) => r.bot_id === b.id) ?? null
    set((s) => {
      const turns = { ...s.turns }
      for (const t of st.turns) {
        const botId = t.bot_id ?? st.bots.find((b) => runs[b.id]?.id === t.run_id)?.id
        if (!botId) continue
        turns[botId] = pruneTurns({ ...(turns[botId] ?? {}), [t.id]: t })
      }
      const selected =
        s.selectedBotId && st.bots.some((b) => b.id === s.selectedBotId)
          ? s.selectedBotId
          : (st.bots[0]?.id ?? null)
      const selectedProject =
        s.selectedProjectId && st.projects.some((p) => p.id === s.selectedProjectId) ? s.selectedProjectId : null
      // SPEC-team：`teams` 以 state 為權威，但保留 WS 已經推進的 phase/usage（state 可能較舊）。
      const teams: Record<string, Team> = {}
      for (const t of st.teams) teams[t.id] = { ...(s.teams[t.id] ?? {}), ...t }
      const selectedTeam = s.selectedTeamId && teams[s.selectedTeamId] ? s.selectedTeamId : null
      return {
        hosts: st.hosts,
        attachCommand: st.attach_command,
        localTools: st.tools,
        localIdentityStatus: st.identity_status,
        identities: st.identities,
        projects: st.projects,
        bots: st.bots,
        teams,
        runs,
        turns,
        connected: st.connected,
        defaultConnected: st.default_connected,
        lastSeq: Math.max(s.lastSeq, st.daemon_seq),
        selectedBotId: selected,
        selectedProjectId: selectedProject,
        selectedTeamId: selectedTeam,
      }
    })
    get().pruneUnread()
    const sel = get().selectedBotId
    if (sel && !get().loadedBots[sel]) await get().loadMessages(sel)
    const proj = get().selectedProjectId
    if (proj && !get().loadedProjects[proj]) await get().loadGroupMessages(proj)
    // team 細節不在這裡重抓：`selectTeam`、`team_changed`（phase 有變）與 `resync` 各自負責，
    // 否則每次 state 刷新都多 2-3 個 team 請求，還會跟刪除賽跑（issue #23）。
  }),

  selectBot: (botId) => {
    set({ selectedBotId: botId, selectedProjectId: null, selectedTeamId: null, teamLaunch: null, shellView: null, rightTab: 'chat', settingsBotId: null })
    // 點進來就是看到了——但只有視窗真的在前景才算（程式化的選取可能發生在背景分頁）。
    if (botId && windowActive()) get().markBotRead(botId)
    if (botId && !get().loadedBots[botId]) void get().loadMessages(botId)
  },

  selectAdjacentBot: (dir) => {
    const next = adjacentBotId(get(), get().selectedBotId, dir)
    if (next) get().selectBot(next)
  },

  selectProject: (projectId) => {
    set({
      selectedProjectId: projectId,
      selectedTeamId: null,
      teamLaunch: null,
      shellView: null,
      rightTab: 'chat',
      settingsBotId: null,
    })
    if (projectId && windowActive()) get().markGroupRead(projectId)
    if (projectId) void get().loadGroupMessages(projectId)
  },

  async loadGroupMessages(projectId) {
    try {
      const startedAt = new Date().toISOString()
      const page = await api.fetchProjectMessages(projectId)
      set((s) => {
        // 同 `loadMessages`：請求飛在半路時進來的 `message_added` 不能被舊的那一頁蓋掉。
        const kept = keptAfterPage(s.groupMessages[projectId] ?? [], page.messages, startedAt)
        return {
          groupMessages: {
            ...s.groupMessages,
            [projectId]: kept.length > 0 ? sortById([...page.messages, ...kept]) : page.messages,
          },
          loadedProjects: { ...s.loadedProjects, [projectId]: true },
          moreMessages: { ...s.moreMessages, [projectId]: page.has_more },
        }
      })
    } catch (e) {
      get().notify('error', `載入群組訊息失敗：${errText(e)}`)
    }
  },

  async loadEarlierGroupMessages(projectId) {
    const s0 = get()
    const oldest = s0.groupMessages[projectId]?.[0]
    if (!oldest || s0.loadingMore[projectId]) return
    set((s) => ({ loadingMore: { ...s.loadingMore, [projectId]: true } }))
    try {
      const page = await api.fetchProjectMessages(projectId, PAGE_SIZE, oldest.id)
      set((s) => {
        const have = new Set((s.groupMessages[projectId] ?? []).map((m) => m.id))
        const older = page.messages.filter((m) => !have.has(m.id))
        return {
          groupMessages: { ...s.groupMessages, [projectId]: [...older, ...(s.groupMessages[projectId] ?? [])] },
          moreMessages: { ...s.moreMessages, [projectId]: page.has_more && older.length > 0 },
        }
      })
    } catch (e) {
      get().notify('error', `載入更早的群組訊息失敗：${errText(e)}`)
    } finally {
      set((s) => ({ loadingMore: withoutKey(s.loadingMore, projectId) }))
    }
  },

  async sendGroupChat(projectId, text, attachments = []) {
    const crid = api.newClientRequestId()
    try {
      const res = await api.sendGroupChat(projectId, text, crid, attachments)
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
  openSettings: (botId, anchor = null) => {
    set({
      selectedBotId: botId,
      selectedProjectId: null,
      rightTab: 'chat',
      settingsBotId: botId,
      settingsAnchor: anchor,
    })
    if (!get().loadedBots[botId]) void get().loadMessages(botId)
  },

  closeSettings: () => set({ settingsBotId: null, settingsAnchor: null }),

  moveBot: (botId, beforeId) => {
    set((s) => {
      const bot = s.bots.find((b) => b.id === botId)
      if (!bot || botId === beforeId) return {}
      const pid = bot.project_id
      const current = botsOfProject(s, pid).map((b) => b.id)
      const rest = current.filter((id) => id !== botId)
      const at = beforeId === null ? rest.length : rest.indexOf(beforeId)
      if (beforeId !== null && at < 0) return {}
      const next = [...rest.slice(0, at), botId, ...rest.slice(at)]
      if (next.join() === current.join()) return {}
      const botOrder = { ...s.botOrder, [pid]: next }
      writeBotOrder(botOrder)
      return { botOrder }
    })
  },

  moveProject: (projectId, beforeId) => {
    set((s) => {
      if (projectId === beforeId || !s.projects.some((p) => p.id === projectId)) return {}
      const current = orderedProjects(s).map((p) => p.id)
      const rest = current.filter((id) => id !== projectId)
      const at = beforeId === null ? rest.length : rest.indexOf(beforeId)
      if (beforeId !== null && at < 0) return {}
      const next = [...rest.slice(0, at), projectId, ...rest.slice(at)]
      if (next.join() === current.join()) return {}
      writeProjectOrder(next)
      return { projectOrder: next }
    })
  },

  requestOpenBotSheet: (projectId) => set({ openBotSheetFor: projectId }),
  clearOpenBotSheet: () => set({ openBotSheetFor: null }),

  async loadMessages(botId) {
    try {
      const startedAt = new Date().toISOString()
      const page = await api.fetchMessages(botId)
      set((s) => {
        const kept = keptAfterPage(s.messages[botId] ?? [], page.messages, startedAt)
        const turns: Record<string, Turn> = {}
        // `sendPrompt` 在這個請求飛出去之後塞的本地 in_flight turn 要留著：它是輸入框的鎖，
        // 被清掉的話下一次送出會撞上 daemon 的 409。頁裡有的還是以頁為準（放棄回合就是這樣
        // 把 in_flight 改掉的）。
        for (const t of Object.values(s.turns[botId] ?? {})) {
          if (t.status === 'in_flight' && t.created_at >= startedAt) turns[t.id] = t
        }
        for (const t of page.turns) turns[t.id] = t
        return {
          messages: { ...s.messages, [botId]: kept.length > 0 ? sortByTime([...page.messages, ...kept]) : page.messages },
          turns: { ...s.turns, [botId]: turns },
          loadedBots: { ...s.loadedBots, [botId]: true },
          moreMessages: { ...s.moreMessages, [botId]: page.has_more },
        }
      })
      // 有訊息可比對了：重整後存下來的數字只是快照，這裡用已讀標記算出真正的未讀回合數。
      get().recountBot(botId)
    } catch (e) {
      get().notify('error', `載入訊息失敗：${errText(e)}`)
    }
  },

  async loadEarlierMessages(botId) {
    const s0 = get()
    const oldest = s0.messages[botId]?.[0]
    if (!oldest || s0.loadingMore[botId]) return
    set((s) => ({ loadingMore: { ...s.loadingMore, [botId]: true } }))
    try {
      const page = await api.fetchMessages(botId, PAGE_SIZE, oldest.id)
      set((s) => {
        const have = new Set((s.messages[botId] ?? []).map((m) => m.id))
        const older = page.messages.filter((m) => !have.has(m.id))
        return {
          messages: { ...s.messages, [botId]: [...older, ...(s.messages[botId] ?? [])] },
          // 補回來的那一頁不再往 `turns` 灌：那個 map 只服務 `inFlightTurn` /
          // `unknownDeliveryTurn`，往回翻的舊回合對它們沒有意義（issue #25 的 `pruneTurns`）。
          moreMessages: { ...s.moreMessages, [botId]: page.has_more && older.length > 0 },
        }
      })
    } catch (e) {
      get().notify('error', `載入更早的訊息失敗：${errText(e)}`)
    } finally {
      set((s) => ({ loadingMore: withoutKey(s.loadingMore, botId) }))
    }
  },

  markBotRead: (botId) => {
    setReadMark(botKey(botId), markOfMessages(get().messages[botId] ?? []) ?? markNow())
    if (!get().botUnread[botId]) return
    set((s) => ({ botUnread: withoutKey(s.botUnread, botId) }))
    persistUnread(get())
  },

  markGroupRead: (projectId) => {
    setReadMark(groupKey(projectId), markOfMessages(get().groupMessages[projectId] ?? []) ?? markNow())
    if (!get().groupUnread[projectId]) return
    set((s) => ({ groupUnread: withoutKey(s.groupUnread, projectId) }))
    persistUnread(get())
  },

  markCurrentRead: () => {
    if (!windowActive()) return
    const s = get()
    if (s.selectedProjectId) {
      get().markGroupRead(s.selectedProjectId)
      return
    }
    if (s.selectedTeamId || s.shellView) return
    if (s.selectedBotId) get().markBotRead(s.selectedBotId)
  },

  pruneUnread: () => {
    const s = get()
    const liveBot = (id: string) => s.bots.some((b) => b.id === id)
    const liveProject = (id: string) => s.projects.some((p) => p.id === id)
    readMarks = pruneMarks(readMarks, liveBot, liveProject)
    saveMarks(readMarks)
    const botUnread = pruneUnread(s.botUnread, liveBot)
    const groupUnread = pruneUnread(s.groupUnread, liveProject)
    if (Object.keys(botUnread).length === Object.keys(s.botUnread).length && Object.keys(groupUnread).length === Object.keys(s.groupUnread).length) return
    set({ botUnread, groupUnread })
    persistUnread(get())
  },

  recountBot: (botId) => {
    const s = get()
    // 正在看著它就不是「未讀」，是「剛剛讀完」——直接把標記推到最後一則。
    if (viewingBot(s, botId) && windowActive()) {
      get().markBotRead(botId)
      return
    }
    const n = countUnreadTurns(s.messages[botId] ?? [], readMarks[botKey(botId)])
    if ((s.botUnread[botId] ?? 0) === n) return
    set((cur) => ({ botUnread: n > 0 ? { ...cur.botUnread, [botId]: n } : withoutKey(cur.botUnread, botId) }))
    persistUnread(get())
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

  /**
   * 強制中止：不管 `esc` 送不送得出去，都把卡住的回合收掉、把輸入框解開。
   * `esc` 沒送成功時講清楚——bot 那頭可能還在跑，只是這邊不再等它。
   */
  async abortBot(botId) {
    await guarded(set, get, `abort:${botId}`, async () => {
      const r = await api.abortBot(botId)
      const n = r.aborted.length
      get().notify(
        r.keys_sent ? 'info' : 'error',
        r.keys_sent
          ? `已中止 ${n} 個回合`
          : `已中止 ${n} 個回合，但 esc 送不進終端——agent 那邊可能還在跑，必要時停掉 Bot`,
      )
    })
  },

  async loginBot(botId) {
    const key = `login:${botId}`
    if (get().busy[key]) return false
    set((s) => ({ busy: { ...s.busy, [key]: true } }))
    try {
      await api.loginBot(botId)
      return true
    } catch (e) {
      get().notify('error', `送不出登入指令：${loginErrText(e)}`)
      return false
    } finally {
      set((s) => {
        const busy = { ...s.busy }
        delete busy[key]
        return { busy }
      })
    }
  },

  queueSend(botId, text, attachments) {
    set((st) => ({ queuedSends: { ...st.queuedSends, [botId]: { text, attachments } } }))
  },

  cancelQueuedSend(botId) {
    set((st) => ({ queuedSends: withoutKey(st.queuedSends, botId) }))
  },

  async sendPrompt(botId, text, attachments = []) {
    const crid = api.newClientRequestId()
    try {
      const res = await api.sendPrompt(botId, text, crid, attachments)
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

  async sendText(botId, text, enter) {
    try {
      await api.sendText(botId, text, enter, get().runs[botId]?.id ?? null)
      return true
    } catch (e) {
      get().notify('error', `送出文字失敗：${errText(e)}`)
      return false
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
      return id || null
    } catch (e) {
      get().notify('error', errText(e))
      return null
    }
  },

  async cloneBot(botId) {
    const s = get()
    const bot = s.bots.find((b) => b.id === botId)
    if (!bot) return null
    const taken = new Set(s.bots.filter((b) => b.project_id === bot.project_id).map((b) => b.name))
    // `claude-cc1` → `claude-cc1-2`；已經是 `-N` 結尾的就往上加。
    const stem = bot.name.replace(/-\d+$/, '') || bot.name
    let n = 2
    while (taken.has(`${stem}-${n}`)) n += 1
    const key = `clone:${botId}`
    set((st) => ({ busy: { ...st.busy, [key]: true } }))
    try {
      const id = await get().addBot(bot.project_id, {
        name: `${stem}-${n}`,
        kind: bot.kind,
        model: bot.model,
        effort: bot.effort,
        fast: bot.fast,
        persona: bot.persona,
        identity: bot.identity,
        env: bot.env,
        autostart: bot.autostart,
        auto_approve: bot.auto_approve,
      })
      // 分身排在本尊後面，而不是掉到清單最尾巴。
      if (id) {
        const ids = botsOfProject(get(), bot.project_id).map((b) => b.id)
        const at = ids.indexOf(botId)
        // 已經緊接在本尊後面就別動；傳 null 在 moveBot 裡是「移到最後」，正好是這裡不要的。
        if (at >= 0 && ids[at + 1] !== id) get().moveBot(id, ids[at + 1] ?? null)
        // Same as the 新增 Bot form, which starts what it just created: a clone is asked for
        // when you want another one of these *now*, so leaving it stopped only adds a click.
        await get().startBot(id)
      }
      return id
    } finally {
      set((st) => ({ busy: { ...st.busy, [key]: false } }))
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
    const name = bot?.name ?? 'Bot'
    try {
      await api.deleteBot(botId)
      // 刪除本來就是軟的（daemon 只設 `deleted_at`，對話全留著），所以復原是真的復原，
      // 不是重新建一個同名的空 bot。
      get().notify('info', `已刪除 ${name}`, {
        label: '復原',
        run: async () => {
          const ok = await api.restoreBot(botId)
          if (ok) {
            await get().refreshState()
            get().selectBot(botId)
          }
        },
      })
      set((s) => {
        const drafts = withoutKey(s.drafts, `bot:${botId}`)
        const draftCursors = withoutKey(s.draftCursors, `bot:${botId}`)
        writeDrafts(drafts)
        writeDraftCursors(draftCursors)
        return {
          selectedBotId: s.selectedBotId === botId ? next : s.selectedBotId,
          settingsBotId: s.settingsBotId === botId ? null : s.settingsBotId,
          drafts,
          draftCursors,
        }
      })
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

  async patchProject(projectId, input) {
    if (Object.keys(input).length === 0) return false
    let ok = false
    try {
      await api.patchProject(projectId, input)
      await get().refreshState()
      ok = true
    } catch (e) {
      get().notify('error', errText(e))
    }
    return ok
  },

  async removeProject(projectId) {
    const botIds = get().bots.filter((b) => b.project_id === projectId).map((b) => b.id)
    try {
      await api.deleteProject(projectId)
      set((s) => {
        let drafts = withoutKey(s.drafts, `group:${projectId}`)
        let draftCursors = withoutKey(s.draftCursors, `group:${projectId}`)
        for (const id of botIds) drafts = withoutKey(drafts, `bot:${id}`)
        for (const id of botIds) draftCursors = withoutKey(draftCursors, `bot:${id}`)
        writeDrafts(drafts)
        writeDraftCursors(draftCursors)
        return {
          drafts,
          draftCursors,
          selectedProjectId: s.selectedProjectId === projectId ? null : s.selectedProjectId,
        }
      })
      await get().refreshState()
    } catch (e) {
      get().notify('error', errText(e))
    }
  },

  async loadQuota() {
    try {
      set({ quota: await api.fetchQuota() })
    } catch {
      /* older daemon without /quota: the strip just shows nothing */
    }
  },

  async loadMem() {
    try {
      set({ mem: await api.fetchMem() })
    } catch {
      /* older daemon without /mem: the header just shows nothing */
    }
  },

  async loadModels(kind, host, identity) {
    // claude 的清單本身不因身份而變，但 `default_effort`（那個身份的 `settings.json`）會，
    // 所以身份要進快取 key，否則切身份不會換到正確的「預設」提示（SPEC §17.1）。
    const key = modelsKey(kind, host, identity)
    const cached = get().models[key]
    // issue #26：失敗不是永久的（主機斷線、CLI 還沒裝、daemon 快取沒暖），
    // null 只擋 MODELS_RETRY_MS，之後再開面板就會重打。
    if (!shouldFetchModels(cached, get().modelsFailedAt[key], Date.now())) return cached ?? null
    try {
      const list = await api.fetchModels(kind, host, identity)
      set((s) => {
        const { [key]: _gone, ...failed } = s.modelsFailedAt
        return { models: { ...s.models, [key]: list }, modelsFailedAt: failed }
      })
      return list
    } catch {
      set((s) => ({
        models: { ...s.models, [key]: null },
        modelsFailedAt: { ...s.modelsFailedAt, [key]: Date.now() },
      }))
      return null
    }
  },

  dropHostModels(host) {
    set((s) => ({
      models: dropHostModels(s.models, host),
      modelsFailedAt: dropHostModels(s.modelsFailedAt, host),
    }))
  },

  async refreshTools(host) {
    const key = `tools:${host || 'local'}`
    set((s) => ({ busy: { ...s.busy, [key]: true } }))
    try {
      const res = await api.refreshTools(host)
      const target = host || 'local'
      if (target === 'local') {
        set({ localTools: res.tools, localIdentityStatus: res.identity_status })
      } else {
        set((s) => ({
          hosts: s.hosts.map((h) =>
            h.name === target ? { ...h, tools: res.tools, identity_status: res.identity_status } : h,
          ),
        }))
      }
      // The CLI may have just been installed: let ModelPicker fetch the real list again.
      get().dropHostModels(target)
      return true
    } catch (e) {
      get().notify('error', `重新偵測 ${host || '本機'} 失敗：${errText(e)}`)
      return false
    } finally {
      set((s) => ({ busy: { ...s.busy, [key]: false } }))
    }
  },

  async installTool(host, kind, viaBotId) {
    const key = `install:${host}:${kind}`
    set((s) => ({ busy: { ...s.busy, [key]: true } }))
    try {
      const res = await api.installTool(host, kind, viaBotId)
      get().selectBot(viaBotId)
      get().notify('info', `已請 ${get().bots.find((b) => b.id === viaBotId)?.name ?? viaBotId} 安裝並登入 ${kind}`)
      return res.turn_id
    } catch (e) {
      get().notify('error', `安裝 ${kind} 失敗：${errText(e)}`)
      return null
    } finally {
      set((s) => ({ busy: { ...s.busy, [key]: false } }))
    }
  },

  setKindDisplay: (mode) => {
    try {
      localStorage.setItem(KIND_DISPLAY_KEY, mode)
    } catch {
      /* private mode etc. */
    }
    set({ kindDisplay: mode })
  },

  dismissToolHint: () => set({ toolHintDismissed: true }),

  setDraft: (key, text) => {
    set((s) => {
      if ((s.drafts[key] ?? '') === text) {
        if (text || !s.draftCursors[key]) return {}
        const draftCursors = withoutKey(s.draftCursors, key)
        writeDraftCursors(draftCursors)
        return { draftCursors }
      }
      const drafts = text ? { ...s.drafts, [key]: text } : withoutKey(s.drafts, key)
      const draftCursors = text ? s.draftCursors : withoutKey(s.draftCursors, key)
      writeDrafts(drafts)
      if (!text) writeDraftCursors(draftCursors)
      return { drafts, draftCursors }
    })
  },

  setDraftCursor: (key, start, end = start) => {
    set((s) => {
      const text = s.drafts[key] ?? ''
      if (!text) {
        if (!s.draftCursors[key]) return {}
        const draftCursors = withoutKey(s.draftCursors, key)
        writeDraftCursors(draftCursors)
        return { draftCursors }
      }
      const max = text.length
      const next: DraftCursor = {
        start: Math.max(0, Math.min(max, Math.floor(start))),
        end: Math.max(0, Math.min(max, Math.floor(end))),
      }
      const previous = s.draftCursors[key]
      if (previous && previous.start === next.start && previous.end === next.end) return {}
      const draftCursors = { ...s.draftCursors, [key]: next }
      writeDraftCursors(draftCursors)
      return { draftCursors }
    })
  },

  readTerminal: (botId, source, lines) => api.fetchTerminal(botId, source, lines),

  // ------------------------------------------------------- 主機 shell

  async openHostShell(host, cwd) {
    if (!get().hostShellSupported) return false
    let ok = false
    await guarded(set, get, `shell:${host}`, async () => {
      try {
        // 先看有沒有活著的：一台主機通常只需要一個 shell，而按第二次「開 shell」想回到的
        // 是剛剛那個終端（裡面還有上一個指令的輸出），不是一片空白。指定了 cwd 就是明確
        // 要「在那個目錄」開一個，這時不接回舊的。
        const existing = cwd ? [] : await api.fetchHostShells(host)
        const reuse = existing.length > 0 ? existing[existing.length - 1] : null
        const shell = reuse ?? (await api.openHostShell(host, cwd))
        // 不動 selectedBotId：shell 掛在目前這個 bot 的標題列底下（2026-09-08），
        // 使用者要的是「在這個 bot 旁邊開個終端」，不是離開對話。
        set({ shellView: { host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null })
        ok = true
      } catch (e) {
        // 缺端點不是失敗，是這版 daemon 沒有這個功能：入口收掉，不跳錯誤。
        if (!api.isHostShellUnsupported(e)) throw e
        set({ hostShellSupported: false })
      }
    })
    return ok
  },

  viewHostShell: (shell) =>
    set({ shellView: { host: shell.host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null }),

  closeShellView: () => set({ shellView: null }),

  async restoreShellView() {
    const v = get().shellView
    if (!v) return
    try {
      const alive = await api.fetchHostShells(v.host)
      if (!alive.some((sh) => sh.pane_id === v.paneId)) set({ shellView: null })
    } catch (e) {
      if (api.isHostShellUnsupported(e)) set({ shellView: null, hostShellSupported: false })
      // 主機暫時連不上就先留著：面板自己會顯示讀取失敗，使用者可以按「關閉」。
    }
  },

  async endHostShell(host, paneId) {
    await guarded(set, get, `shell:${host}:${paneId}`, async () => {
      await api.closeHostShell(host, paneId)
      set((s) =>
        s.shellView && s.shellView.host === host && s.shellView.paneId === paneId ? { shellView: null } : {},
      )
    })
  },

  // ------------------------------------------------------------ SPEC-team

  selectTeam: (teamId) => {
    set((s) => ({
      selectedTeamId: teamId,
      selectedProjectId: null,
      teamLaunch: null,
      shellView: null,
      rightTab: 'chat',
      settingsBotId: null,
      teamUnread: teamId ? { ...s.teamUnread, [teamId]: 0 } : s.teamUnread,
    }))
    if (teamId) void get().loadTeam(teamId)
  },

  async loadTeam(teamId) {
    if (!get().teamsSupported) return
    // The timeline is `GET /projects/:id/messages` filtered by `team_id` (SPEC-team §11.3),
    // so the project's merged history has to be there before the panel can render anything.
    const projectId = get().teams[teamId]?.project_id ?? get().teamDetail[teamId]?.project_id ?? null
    if (projectId && !get().loadedProjects[projectId]) await get().loadGroupMessages(projectId)
    try {
      const [detail, events] = await Promise.all([api.fetchTeam(teamId), api.fetchTeamEvents(teamId)])
      // 舊 daemon 的 SPA fallback 會對 `/api/teams/:id` 回 200 + index.html，解不出 team。
      if (!detail && !get().teams[teamId]) {
        set({ teamsSupported: false, teamLaunch: null, selectedTeamId: null })
        return
      }
      set((s) => ({
        teamDetail: detail ? { ...s.teamDetail, [teamId]: detail } : s.teamDetail,
        teams: detail ? { ...s.teams, [teamId]: { ...(s.teams[teamId] ?? {}), ...stripDetail(detail) } } : s.teams,
        teamEvents: { ...s.teamEvents, [teamId]: capList(events, TEAM_EVENT_CAP).list },
      }))
      const pid = detail?.project_id
      if (pid && !get().loadedProjects[pid]) await get().loadGroupMessages(pid)
    } catch (e) {
      if (markTeamsUnsupported(set, get, e)) return
      if (api.isTeamNotFound(e)) {
        // 這個 team 已經不在了（多半是剛被刪掉——`refreshState` 尾巴的重載會跟刪除賽跑，
        // 別的視窗刪的也一樣）。靜靜清掉本地痕跡就好，不要跳錯誤。
        set((s) => forgetTeamPatch(s, teamId, true))
        return
      }
      get().notify('error', `載入 Team 失敗：${errText(e)}`)
    }
  },

  openTeamLaunch: (projectId, issueNumber, repo = '') =>
    set({ teamLaunch: { projectId, issueNumber, repo }, selectedTeamId: null, shellView: null, settingsBotId: null }),

  closeTeamLaunch: () => set({ teamLaunch: null }),

  async createTeam(projectId, input) {
    try {
      const id = await api.createTeam(projectId, input)
      set({ teamLaunch: null })
      await get().refreshState()
      if (id) {
        get().selectTeam(id)
        get().notify('info', `已建立 Team（${input.issue_numbers.map((n) => `#${n}`).join('、')}），成員啟動中…`)
      }
      return id || null
    } catch (e) {
      if (markTeamsUnsupported(set, get, e)) return null
      get().notify('error', `建立 Team 失敗：${errText(e)}`)
      return null
    }
  },

  async controlTeam(teamId, action) {
    let ok = false
    await guarded(set, get, `team:${teamId}:${action}`, async () => {
      await api.controlTeam(teamId, action)
      ok = true
      // `cleanup` 收現場但**留下 `teams` 這一列**（SPEC-team §6.5），所以 row 不動，
      // 只丟掉細節與草稿；`delete` 才是連紀錄一起移除的那條路（§6.5a，見 `removeTeam`）。
      if (action === 'cleanup') set((s) => forgetTeamPatch(s, teamId, false))
      await get().refreshState()
    })
    return ok
  },

  async removeTeam(teamId, branches = 'keep') {
    const team = get().teams[teamId]
    let ok = false
    await guarded(set, get, `team:${teamId}:delete`, async () => {
      try {
        await api.deleteTeam(teamId, branches)
      } catch (e) {
        // 舊 daemon 根本沒有這個端點（裸 404 / 405）→ 靜默關掉整組 team UI。
        if (markTeamsUnsupported(set, get, e)) return
        // 冪等（§6.5a）：已經不在了就當成功——本地照樣清乾淨，不要跳錯誤。
        if (!api.isTeamNotFound(e)) throw e
      }
      ok = true
      set((s) => forgetTeamPatch(s, teamId, true))
      await get().refreshState()
      const label = team ? `Team #${team.issue_number}` : 'Team'
      get().notify('info', branches === 'delete' ? `已刪除 ${label}（含分支）` : `已刪除 ${label}（分支保留）`)
    })
    return ok
  },

  async patchTeam(teamId, input) {
    let ok = false
    await guarded(set, get, `team:${teamId}:patch`, async () => {
      await api.patchTeam(teamId, input)
      ok = true
      await get().loadTeam(teamId)
      await get().refreshState()
    })
    return ok
  },

  async closeTeamIssue(teamId, issueId) {
    const team = get().teams[teamId]
    let ok = false
    await guarded(set, get, `team:${teamId}:close-issue`, async () => {
      const out = await api.closeTeamIssue(teamId, issueId ? { issue_id: issueId } : undefined)
      ok = true
      await get().loadTeam(teamId)
      await get().refreshState()
      const n = out.number || team?.issue_number || 0
      get().notify(
        'info',
        out.already_closed ? `issue #${n} 本來就已經關閉了` : `已關閉 issue #${n}`,
      )
    })
    return ok
  },

  async sayToTeam(teamId, text, to) {
    try {
      await api.sayToTeam(teamId, text, to, api.newClientRequestId())
      return true
    } catch (e) {
      if (markTeamsUnsupported(set, get, e)) return false
      get().notify('error', errText(e))
      return false
    }
  },

  async answerTeam(teamId, text) {
    try {
      await api.answerTeam(teamId, text)
      await get().loadTeam(teamId)
      return true
    } catch (e) {
      if (markTeamsUnsupported(set, get, e)) return false
      get().notify('error', errText(e))
      return false
    }
  },

  async decideTeamTask(teamId, taskId, action, note) {
    let ok = false
    await guarded(set, get, `team:${teamId}:decide:${taskId}`, async () => {
      await api.decideTeamTask(teamId, taskId, action, note)
      ok = true
      await get().loadTeam(teamId)
    })
    return ok
  },
  async addTeamIssues(teamId, issueNumbers) {
    const key = `team:${teamId}:add-issues`
    if (get().busy[key]) return false
    let ok = false
    set((s) => ({ busy: { ...s.busy, [key]: true } }))
    try {
      await api.addTeamIssues(teamId, issueNumbers)
      ok = true
      set((s) => ({ teamReopenUnavailable: withoutKey(s.teamReopenUnavailable, teamId) }))
      await get().loadTeam(teamId)
      get().notify('info', `已加入佇列：${issueNumbers.map((n) => `#${n}`).join('、')}`)
    } catch (e) {
      if (markTeamsUnsupported(set, get, e)) return false
      // §2.5.1：cleanup 過的 done team 永遠回這個 409，重試也沒用——記住它，把按鈕收掉。
      if (e instanceof ApiError && e.status === 409 && e.body.reason === 'team is cleaned up') {
        set((s) => ({ teamReopenUnavailable: { ...s.teamReopenUnavailable, [teamId]: true } }))
        get().notify('info', '這個 Team 已經清理，無法追加 issue。')
      } else if (e instanceof ApiError && e.status === 409 && e.body.reason === 'issue already queued') {
        // §2.3：只有還在佇列上（待處理 / 進行中）的同號 issue 會擋。原文是 `issue already
        // queued`，對使用者只是一句英文——直接說是哪一號、以及它已經在佇列裡了。
        const n = typeof e.body.issue_number === 'number' ? e.body.issue_number : null
        get().notify('info', n === null ? '這個 issue 已在佇列裡。' : `#${n} 已在佇列裡。`)
      } else {
        get().notify('error', `追加 issue 失敗：${errText(e)}`)
      }
    } finally {
      set((s) => {
        const busy = { ...s.busy }
        delete busy[key]
        return { busy }
      })
    }
    return ok
  },
  async removeTeamIssue(teamId, issueId) {
    let ok = false
    await guarded(set, get, `team:${teamId}:remove-issue:${issueId}`, async () => {
      await api.removeTeamIssue(teamId, issueId)
      ok = true
      await get().loadTeam(teamId)
    })
    return ok
  },
}))

/**
 * 抹掉某個 team 的本地痕跡（`cleanup` / `delete` / WS `deleted:true` 共用）。
 *
 * `dropRow` 才把 `teams` 這一列拿掉：`cleanup` 在 daemon 端會保留 row（SPEC-team §6.5），
 * 本地先刪掉只會讓節點閃一下又被 `refreshState` 補回來；`delete`（§6.5a）才是真的沒了。
 * 正選著這個 team 時把選取放掉 —— `refreshState` 會把選取退回既有的 bot，
 * 不會留在一個已經不存在的 team 上變成白畫面。
 */
function forgetTeamPatch(s: StoreState, teamId: string, dropRow: boolean): Partial<StoreState> {
  const drafts = withoutKey(s.drafts, `team:${teamId}`)
  const draftCursors = withoutKey(s.draftCursors, `team:${teamId}`)
  writeDrafts(drafts)
  writeDraftCursors(draftCursors)
  return {
    selectedTeamId: s.selectedTeamId === teamId ? null : s.selectedTeamId,
    ...(dropRow ? { teams: withoutKey(s.teams, teamId) } : {}),
    teamDetail: withoutKey(s.teamDetail, teamId),
    teamEvents: withoutKey(s.teamEvents, teamId),
    teamUnread: withoutKey(s.teamUnread, teamId),
    teamReopenUnavailable: withoutKey(s.teamReopenUnavailable, teamId),
    drafts,
    draftCursors,
  }
}

/** `TeamDetail` 的 `Team` 部分（`teams` map 只存共同欄位，細節留在 `teamDetail`）。 */
function stripDetail(detail: TeamDetail): Team {
  const { tasks: _tasks, summary: _summary, base_ref: _ref, base_sha: _sha, worktree_root: _root, ...team } = detail
  return team
}

/**
 * daemon 還沒有 team 端點（404 / 405）→ 靜默關掉整組 team UI，只留一則說明用的 info。
 * 回 true 代表「已處理，呼叫端不要再跳錯誤」。
 */
function markTeamsUnsupported(set: SetFn, get: GetFn, e: unknown): boolean {
  if (!api.isTeamsUnsupported(e)) return false
  if (get().teamsSupported) {
    set({ teamsSupported: false, teamLaunch: null, selectedTeamId: null })
    get().notify('info', '這個 daemon 版本還沒有 Team 端點，已隱藏「組隊」功能。')
  }
  return true
}

// One subscription instead of a write at every mutation site: `selectBot`, `selectProject`,
// `openSettings`, `addBot`, `removeBot`/`removeProject` and `refreshState`'s own "the selected
// bot is gone" fallback are all covered — as is anything added later.
let lastSelection = initialSelection
let lastShellView = initialShellView
useStore.subscribe((s) => {
  if (s.shellView !== lastShellView) {
    lastShellView = s.shellView
    writeShellView(s.shellView)
  }
  if (
    s.selectedBotId === lastSelection.botId &&
    s.selectedProjectId === lastSelection.projectId &&
    s.selectedTeamId === lastSelection.teamId
  ) {
    return
  }
  lastSelection = { botId: s.selectedBotId, projectId: s.selectedProjectId, teamId: s.selectedTeamId }
  writeSelection(lastSelection)
})

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

/** 使用者現在看的是不是這個 bot 的對話（群組 / team / shell 都會蓋掉它）。 */
function viewingBot(s: StoreState, botId: string): boolean {
  return s.selectedBotId === botId && !s.selectedProjectId && !s.selectedTeamId && !s.shellView
}

/**
 * 一個回合完成了：assistant 訊息到達、或 `turn_updated` 進終態——同一個回合這兩件事都會
 * 發生，`takeTurnCompletion` 負責只算一次。使用者當下不在看（選的是別的 bot、分頁在背景、
 * 或視窗沒 focus）就記成未讀，等他真的點進來再清。
 */
function noteTurnDone(set: SetFn, get: GetFn, botId: string, turnId: string) {
  if (viewingBot(get(), botId) && windowActive()) {
    get().markBotRead(botId)
    return
  }
  if (!takeTurnCompletion(botId, turnId)) return
  set((s) => ({ botUnread: { ...s.botUnread, [botId]: (s.botUnread[botId] ?? 0) + 1 } }))
  persistUnread(get())
}

/** 同上，但帳記在專案的群組聊天上（§13.6 的未讀本來就是以 project 為單位）。 */
function noteGroupTurnDone(set: SetFn, get: GetFn, projectId: string, turnId: string) {
  if (get().selectedProjectId === projectId && windowActive()) {
    get().markGroupRead(projectId)
    return
  }
  if (!takeTurnCompletion(`group:${projectId}`, turnId)) return
  set((s) => ({ groupUnread: { ...s.groupUnread, [projectId]: (s.groupUnread[projectId] ?? 0) + 1 } }))
  persistUnread(get())
}

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
          const team = get().selectedTeamId
          if (team) await get().loadTeam(team)
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
        if (pick(data, 'default_connected') !== undefined) {
          patch.defaultConnected = bool(pick(data, 'default_connected'), s.defaultConnected)
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
      // issue #26: a host that just came back may now answer `GET /api/models`.
      const wasConnected = name === 'local'
        ? get().connected
        : get().hosts.find((h) => h.name === name)?.connected ?? false
      if (!wasConnected && bool(pick(data, 'connected'), false)) get().dropHostModels(name)
      if (name === 'local') {
        // The reserved local entry maps onto `connected`, not the hosts list.
        set({
          connected: bool(pick(data, 'connected'), get().connected),
          ...(pick(data, 'tools') !== undefined ? { localTools: toToolMap(pick(data, 'tools')) } : {}),
          ...(pick(data, 'identities') !== undefined
            ? { localIdentityStatus: toIdentityStatusMap(pick(data, 'identities')) }
            : {}),
        })
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
      const record = isRec(data) ? data : null
      const run = toRun(record ? (record.run ?? null) : null, botId)
      const bot = get().bots.find((b) => b.id === botId)
      const defaultSession = str(record ? pick(record, 'herdr_session') : undefined) === 'default' || bot?.herdr_session === 'default'
      // bot_status.connected is the state of the host/session the bot lives on (#20). A remote
      // bot must never flip the global herdr flag; it only patches its own host entry.
      const host = str(record ? pick(record, 'host') : undefined) || projectHostName(get(), bot?.project_id ?? null)
      const target = botStatusConnTarget({
        connected: record && record.connected !== undefined ? bool(record.connected, true) : undefined,
        host,
        defaultSession,
      })
      set((s) => ({
        runs: { ...s.runs, [botId]: run },
        ...(target.kind === 'global' ? { connected: target.connected } : {}),
        ...(target.kind === 'default' ? { defaultConnected: target.connected } : {}),
        ...(target.kind === 'host'
          ? { hosts: mergeHosts(s.hosts, [{ name: target.host, connected: target.connected }]) }
          : {}),
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
        const more: Record<string, boolean> = {}
        // issue #25：接在尾端（幾乎永遠如此）就只是 push，不再對整包重排；滿了就從頭截掉，
        // 並把「還有更早的」打開，讓使用者自己分頁補回來。
        const grown = insertSorted(s.messages[botId] ?? [], msg, byTime)
        if (grown) {
          const cut = capList(grown, MESSAGE_CAP)
          patch.messages = { ...s.messages, [botId]: cut.list }
          if (cut.trimmed) more[botId] = true
        }
        // The final reply supersedes the live (partial) one.
        if (msg.role === 'assistant' && s.liveReply[botId] && (!msg.turn_id || s.liveReply[botId].turnId === msg.turn_id)) {
          patch.liveReply = withoutKey(s.liveReply, botId)
        }
        // §13: route the same frame into its project's group timeline + unread counter.
        const bot = s.bots.find((b) => b.id === botId)
        if (bot) {
          const pid = bot.project_id
          const group = s.groupMessages[pid]
          const grownGroup = group ? insertSorted(group, { ...msg, bot_id: botId, bot_name: bot.name }, byId) : null
          if (grownGroup) {
            const cut = capList(grownGroup, MESSAGE_CAP)
            patch.groupMessages = { ...s.groupMessages, [pid]: cut.list }
            if (cut.trimmed) more[pid] = true
          }
        }
        // SPEC-team §11.5: the team timeline reads the same `groupMessages` rows (filtered by
        // `team_id`), so only the unread counter is team-specific here.
        if (msg.team_id && msg.role !== 'user' && s.selectedTeamId !== msg.team_id) {
          patch.teamUnread = { ...s.teamUnread, [msg.team_id]: (s.teamUnread[msg.team_id] ?? 0) + 1 }
        }
        if (Object.keys(more).length > 0) patch.moreMessages = { ...s.moreMessages, ...more }
        return patch
      })
      // 一則 assistant 訊息 = 一個回合完成。記未讀要在 set 之後：`markBotRead` 的標記是從
      // 已經含這則訊息的清單推出來的。team 成員也是 bot，走的是同一條路。
      if (completesTurn(msg)) {
        // 同一個回合只記一次：沒有 `turn_id` 的訊息也要跟 `turn_updated` 落在同一個 key 上，
        // 否則這裡記 `msg:<id>`、回合終態再記 `turn.id`，一則回覆讓徽章跳兩下。
        const turnId = completionKey(msg, Object.keys(get().turns[botId] ?? {}))
        noteTurnDone(set, get, botId, turnId)
        const pid = get().bots.find((b) => b.id === botId)?.project_id
        if (pid) noteGroupTurnDone(set, get, pid, turnId)
      }
      return
    }
    case 'turn_updated': {
      const botId = frameBotId(data)
      const turn = toTurn(unwrap(data, 'turn'), botId ?? undefined)
      if (!turn || !botId) return
      set((s) => ({
        // issue #25：這個 map 只有 `inFlightTurn` / `unknownDeliveryTurn` 兩個讀者，
        // 收掉的舊回合沒人再看——沒選到的 bot 更是只增不減，所以每次都順手剪一下。
        turns: { ...s.turns, [botId]: pruneTurns({ ...(s.turns[botId] ?? {}), [turn.id]: turn }) },
        liveReply:
          turn.status !== 'in_flight' && s.liveReply[botId]?.turnId === turn.id ? withoutKey(s.liveReply, botId) : s.liveReply,
      }))
      // The turn that was blocking the composer is over: send whatever was queued behind it.
      if (turn.status !== 'in_flight') {
        flushQueued(botId)
        // 沒有 assistant 訊息的回合（被中止、只有終端輸出）也要算完成，否則它永遠不會亮。
        noteTurnDone(set, get, botId, turn.id)
        const pid = get().bots.find((b) => b.id === botId)?.project_id
        if (pid) noteGroupTurnDone(set, get, pid, turn.id)
      }
      return
    }
    case 'turn_progress': {
      // API.md v3.9/v4.1/v4.2: `{bot_id, run_id, turn_id, text, activity?, alert?, revision}` —
      // the partial reply so far, the spinner row (`activity`) for turns that are still only
      // thinking, and any retry / API-error banner (`alert`).
      const botId = frameBotId(data)
      if (!botId || !isRec(data)) return
      const turnId = str(pick(data, 'turn_id', 'turnId'))
      if (!turnId) return
      const text = str(pick(data, 'text', 'content'))
      const activity = str(pick(data, 'activity'))
      const alert = str(pick(data, 'alert'))
      const revision = Number(pick(data, 'revision') ?? 0) || 0
      // 多個 agent 同時串流時每秒進來好幾個 frame；每個都 set 就每個都 render。同一個 bot
      // 250 ms 內只留最後一個（頭一個立刻套用，之後的合併到下一拍），畫面看不出差別。
      const pendingKey = botId
      const apply = () =>
        set((s) => {
          const prev = s.liveReply[botId]
          // Frames can only move forward within a turn; a new turn always replaces.
          if (prev && prev.turnId === turnId && prev.revision > revision) return {}
          return { liveReply: { ...s.liveReply, [botId]: { turnId, text, activity, alert, revision } } }
        })
      const slot = liveThrottle.get(pendingKey)
      if (slot) {
        slot.apply = apply
        return
      }
      apply()
      const entry = { apply: null as null | (() => void) }
      liveThrottle.set(pendingKey, entry)
      setTimeout(() => {
        liveThrottle.delete(pendingKey)
        entry.apply?.()
      }, LIVE_THROTTLE_MS)
      return
    }
    case 'mem_updated': {
      set({ mem: toMemSnapshot(data) })
      return
    }
    case 'quota_updated': {
      // v4.0: `{kind, host, quota}` (or a whole `{kinds}` map). `kind` 是完整的 map key，
      // 遠端主機帶 `<host>/` 前綴（SPEC §14）。
      if (!isRec(data)) return
      const kinds = pick(data, 'kinds')
      if (isRec(kinds)) {
        set((s) => ({
          quota: { ...s.quota, ...Object.fromEntries(Object.entries(kinds).map(([k, v]) => [k, toKindQuota(v, k)])) },
        }))
        return
      }
      const kind = str(pick(data, 'kind'))
      if (!kind) return
      set((s) => ({ quota: { ...s.quota, [kind]: toKindQuota(pick(data, 'quota'), kind) } }))
      return
    }
    case 'team_changed': {
      // SPEC-team §10.6: `{team_id, project_id, phase, pause_reason, usage}` — a partial patch.
      if (!isRec(data)) return
      const teamId = str(pick(data, 'team_id', 'id'))
      if (!teamId) return
      if (bool(pick(data, 'deleted'), false)) {
        // SPEC-team §6.5a：刪除完成的那一則。節點要立刻消失（成員的 `bot_changed` 會另外來），
        // 而且不能 `refreshState` 把它撈回來——這一列在 daemon 端已經不存在了。
        set((s) => forgetTeamPatch(s, teamId, true))
        void get().refreshState()
        return
      }
      // §6.5a 的「刪除中」phase 不在 `TeamPhase` 裡，`toTeam` 的 `oneOf` 會 fallback 成
      // `starting`，節點反而顯示「啟動中」。刪除完成馬上會推 `deleted:true`，這一則略過。
      if (str(pick(data, 'phase')) === 'deleting') return
      const existing = get().teams[teamId]
      if (!existing) {
        // A team this client has not seen yet (just created elsewhere): pull the full record.
        void get().refreshState()
        return
      }
      const merged = toTeam({ ...existing, ...data, id: teamId }, existing.project_id)
      if (!merged) return
      set((s) => ({
        teams: { ...s.teams, [teamId]: merged },
        teamDetail: s.teamDetail[teamId] ? { ...s.teamDetail, [teamId]: { ...s.teamDetail[teamId], ...merged } } : s.teamDetail,
      }))
      if (existing.phase !== merged.phase) {
        if (TEAM_TERMINAL_PHASES.includes(merged.phase)) {
          get().notify('info', `Team #${merged.issue_number} ${TEAM_PHASE_LABEL[merged.phase]}`)
        }
        // `summary` / `base_*` / `worktree_root` 只在 `GET /teams/:id` 上，phase 一動就重抓
        // （只對開著的 team，不會變成每則事件一次請求）。
        if (get().selectedTeamId === teamId) void get().loadTeam(teamId)
      }
      return
    }
    case 'team_task_updated': {
      if (!isRec(data)) return
      const teamId = str(pick(data, 'team_id'))
      const task = toTeamTask(unwrap(data, 'task'))
      if (!teamId || !task) return
      set((s) => {
        const detail = s.teamDetail[teamId]
        if (!detail) return {}
        const tasks = detail.tasks.some((t) => t.id === task.id)
          ? detail.tasks.map((t) => (t.id === task.id ? task : t))
          : [...detail.tasks, task]
        return { teamDetail: { ...s.teamDetail, [teamId]: { ...detail, tasks: tasks.sort((a, b) => a.seq - b.seq) } } }
      })
      return
    }
    case 'team_event': {
      if (!isRec(data)) return
      const teamId = str(pick(data, 'team_id'))
      const ev = toTeamEvent(unwrap(data, 'event'))
      if (!teamId || !ev) return
      set((s) => {
        const list = s.teamEvents[teamId]
        if (!list) return {}
        if (list.some((x) => x.id === ev.id)) return {}
        // issue #25：時間軸只往後長。這裡沒有分頁可以補，舊事件就直接丟。
        return { teamEvents: { ...s.teamEvents, [teamId]: capList([...list, ev], TEAM_EVENT_CAP).list } }
      })
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

function withoutKey<T>(map: Record<string, T>, key: string): Record<string, T> {
  const { [key]: _dropped, ...rest } = map
  return rest
}

/**
 * Send the message queued behind a turn that has just finished.
 *
 * The short delay lets the rest of the frame land (`run_updated` may still flip
 * `agent_status`), and the composer state is re-checked at the last moment so a bot that
 * went `blocked` — or that already has another turn in flight — keeps the queued text
 * instead of losing it to a 409.
 */
function flushQueued(botId: string) {
  setTimeout(() => {
    const s = useStore.getState()
    const pending = s.queuedSends[botId]
    if (!pending) return
    const cs = composerState(s, botId)
    if (cs.disabled || cs.queued) return
    useStore.setState({ queuedSends: withoutKey(s.queuedSends, botId) })
    void s.sendPrompt(botId, pending.text, pending.attachments)
  }, 350)
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
    const tools = u.tools !== undefined ? toToolMap(u.tools) : h.tools
    // `host_changed` only carries `identities` when the daemon has a detection result; an
    // absent field means "unchanged", never "no identities".
    const identityStatus = u.identities !== undefined ? toIdentityStatusMap(u.identities) : h.identity_status
    if (connected === h.connected && error === h.error && tools === h.tools && identityStatus === h.identity_status) {
      return h
    }
    changed = true
    return { ...h, connected, error, tools, identity_status: identityStatus }
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
  const connected = bot?.herdr_session === 'default' ? state.defaultConnected : state.connected
  return lampOf(state.runs[botId], connected)
}

/** 哪一個視窗在警告——只用來標文字，不是判斷門檻。 */
export type QuotaWarningWindow = '5h' | '7d' | '週'

export interface QuotaWarning {
  critical: boolean
  /** 觸發 critical 的那個視窗剩餘 %（兩個都 critical 時取剩得比較少的那個）。 */
  pct: number
  window: QuotaWarningWindow
}

/**
 * 這個 bot 用的那組額度是不是快用完了。門檻本身不在這裡判斷——`critical` 是 daemon 算好
 * 直接讀旗標（見 docs/API.md §12.4）；這裡只是把 bot 對到它的額度 key，再挑出要顯示的
 * 視窗與剩餘 %（純顯示用的數字，不是另一次門檻判斷）。
 * 側欄 bot 列用來決定要不要整列反灰、警語要寫什麼。
 */
export function botQuotaWarning(
  quota: QuotaMap,
  kind: BotKind,
  identity: string | null,
  host: string = LOCAL_HOST,
): QuotaWarning | null {
  // 額度按主機分（SPEC §14）：遠端 bot 只看它自己那台的數字。
  const scoped = (base: string) => quotaKey(host, base)
  let q = quota[scoped(identity ? `${kind}:${identity}` : kind)]
  // cc0／空 env 的預設身份可能沒有自己的 key，額度會落在裸的 kind 上（同 QuotaStrip 的規則）。
  if (q == null && identity === 'cc0') q = quota[scoped(kind)]
  if (!q) return null
  const five = q.five_hour?.critical ? { pct: Math.max(0, Math.round(100 - q.five_hour.used_pct)), window: '5h' as const } : null
  const sevenWindow: QuotaWarningWindow = kind === 'grok' ? '週' : '7d'
  const seven = q.seven_day?.critical
    ? { pct: Math.max(0, Math.round(100 - q.seven_day.used_pct)), window: sevenWindow }
    : null
  if (!five && !seven) return null
  const worse = five && seven ? (five.pct <= seven.pct ? five : seven) : (five ?? seven)!
  return { critical: true, pct: worse.pct, window: worse.window }
}

/** 側欄身份徽章用：低額度時跟頂端 QuotaStrip 一樣變黃／變紅，並帶上剩餘 %。 */
export interface QuotaLevel {
  level: 'warn' | 'crit'
  /** 觸發的那個視窗剩餘 %（多個同時觸發時取剩最少的）。 */
  pct: number
  window: string
}

/**
 * 這個 bot 用的那組額度目前的燈號。跟 `botQuotaWarning` 的差別是連 `low`（黃）也算——
 * 頂端 QuotaStrip 已經黃了，側欄的 `cc1` 卻還是灰的，看起來像兩套數字。門檻一樣由 daemon
 * 決定（docs/API.md §12.4），這裡只挑出要顯示的視窗與剩餘 %。
 */
export function botQuotaLevel(
  quota: QuotaMap,
  kind: BotKind,
  identity: string | null,
  host: string = LOCAL_HOST,
): QuotaLevel | null {
  const scoped = (base: string) => quotaKey(host, base)
  let q = quota[scoped(identity ? `${kind}:${identity}` : kind)]
  if (q == null && identity === 'cc0') q = quota[scoped(kind)]
  if (!q) return null
  const pick = (w: { used_pct: number; low: boolean; critical: boolean } | null | undefined, name: string) =>
    w && (w.low || w.critical)
      ? { level: (w.critical ? 'crit' : 'warn') as 'warn' | 'crit', pct: Math.max(0, Math.round(100 - w.used_pct)), window: name }
      : null
  const hits = [pick(q.five_hour, '5h'), pick(q.seven_day, kind === 'grok' ? '週' : '7d'), pick(q.fable, 'F')].filter(
    (x): x is QuotaLevel => x !== null,
  )
  if (hits.length === 0) return null
  // 紅優先於黃；同色取剩最少的那個視窗。
  return hits.sort((a, b) => (a.level === b.level ? a.pct - b.pct : a.level === 'crit' ? -1 : 1))[0]
}

/**
 * 側欄的專案，套上使用者拖出來的順序；`projectOrder` 沒列到的（剛新增的）依 daemon 的順序接在後面。
 */
export function orderedProjects(state: { projects: Project[]; projectOrder: string[] }): Project[] {
  if (state.projectOrder.length === 0) return state.projects
  const rank = new Map(state.projectOrder.map((id, i) => [id, i]))
  const known = state.projects.filter((p) => rank.has(p.id)).sort((a, b) => rank.get(a.id)! - rank.get(b.id)!)
  const added = state.projects.filter((p) => !rank.has(p.id))
  return [...known, ...added]
}

/**
 * 某個 project 的 bot，套上使用者拖曳出來的順序。`botOrder` 裡沒有的（剛新增的）
 * 依 daemon 回來的順序接在後面，所以拖過的清單不會因為新增 bot 而重排。
 */
export function botsOfProject(
  state: { bots: Bot[]; botOrder: Record<string, string[]> },
  projectId: string,
): Bot[] {
  const list = state.bots.filter((b) => b.project_id === projectId)
  const order = state.botOrder[projectId]
  if (!order || order.length === 0) return list
  const rank = new Map(order.map((id, i) => [id, i]))
  const known = list.filter((b) => rank.has(b.id)).sort((a, b) => rank.get(a.id)! - rank.get(b.id)!)
  const added = list.filter((b) => !rank.has(b.id))
  return [...known, ...added]
}

/**
 * Every bot id in the order the sidebar paints them: projects top to bottom, and inside each
 * the user's own drag order. This is what ↑/↓ walks — the visual order, not `bots[]`.
 */
export function orderedBotIds(state: {
  projects: Project[]
  projectOrder: string[]
  bots: Bot[]
  botOrder: Record<string, string[]>
}): string[] {
  const out: string[] = []
  for (const p of orderedProjects(state)) for (const b of botsOfProject(state, p.id)) out.push(b.id)
  // A bot whose project vanished from the list would otherwise be unreachable by keyboard.
  for (const b of state.bots) if (!out.includes(b.id)) out.push(b.id)
  return out
}

/** The neighbour `dir` steps away, wrapping at both ends; null when there is nothing to move to. */
export function adjacentBotId(
  state: { projects: Project[]; projectOrder: string[]; bots: Bot[]; botOrder: Record<string, string[]> },
  from: string | null,
  dir: -1 | 1,
): string | null {
  const ids = orderedBotIds(state)
  if (ids.length === 0) return null
  const at = from ? ids.indexOf(from) : -1
  // Nothing selected yet: ↓ starts at the top, ↑ at the bottom.
  if (at < 0) return dir === 1 ? ids[0] : ids[ids.length - 1]
  return ids[(at + dir + ids.length) % ids.length]
}

/**
 * 一個 bot 身上「可以被搜到」的全部文字。
 *
 * 搜尋要能用你**記得的任何一件事**找到它——不只是名字。實務上你會記得的是「那個在 pt 上
 * 跑 opus 的」「那個 reviewer」「那個標題寫著資料夾選擇的」，所以專案、主機、kind、身分、
 * 模型、人設、agent 目前的標題全都算進去。
 */
export function botSearchText(state: StoreState, bot: Bot): string {
  const project = state.projects.find((p) => p.id === bot.project_id)
  const run = state.runs[bot.id] ?? null
  return [
    bot.name,
    bot.kind,
    bot.identity ?? '預設',
    bot.model ?? '',
    bot.persona ?? '',
    run?.agent_title ?? '',
    bot.team?.role ?? '',
    project?.label ?? '',
    project?.path ?? '',
    project?.host === LOCAL_HOST ? '本機 local' : (project?.host ?? ''),
  ]
    .join(' ')
    .toLowerCase()
}

/**
 * 每個字（以空白分隔）都要命中，順序不拘：`opus pt` 找得到「pt 專案裡跑 opus 的那個」。
 * 空字串代表沒有在搜尋，一律視為命中。
 */
export function botMatches(state: StoreState, bot: Bot, query: string): boolean {
  const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean)
  if (terms.length === 0) return true
  const hay = botSearchText(state, bot)
  return terms.every((t) => hay.includes(t))
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
  const base: ComposerState = { disabled: true, reason: '', queued: false, inFlightTurnId: null, unknownTurnId: null }
  if (!botId) return { ...base, reason: '請先在左側選擇一個 Bot' }
  const bot = state.bots.find((b) => b.id === botId)
  const hostName = bot ? projectHostName(state, bot.project_id) : 'local'
  if (hostName !== 'local') {
    const host = hostOfBot(state, botId)
    if (!host || !host.connected) {
      return { ...base, reason: `主機未連線（${hostName}）${host?.error ? `：${host.error}` : ''}` }
    }
  } else if (!(bot?.herdr_session === 'default' ? state.defaultConnected : state.connected)) {
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
    // Not `disabled`: the user keeps typing, and a send is queued rather than refused.
    return { ...base, disabled: false, queued: true, reason: '這回合還在跑，送出會排到結束後', inFlightTurnId: inflight.id }
  }
  return { disabled: false, reason: '', queued: false, inFlightTurnId: null, unknownTurnId: null }
}

const NO_TOOLS: ToolMap = toToolMap(undefined)
const NO_IDENTITY_STATUS: IdentityStatusMap = {}

/** v4.0: the tools map for a host name (`local` = the daemon's machine). Stable references. */
export function toolsOfHost(state: StoreState, host: string): ToolMap {
  if (!host || host === 'local') return state.localTools
  return state.hosts.find((h) => h.name === host)?.tools ?? NO_TOOLS
}

/**
 * v4.0: per-identity login state **on one host**. An identity is global config, but whether
 * its account is usable is a property of the machine the bot will run on, so every caller
 * has to say which host it means. Stable references (safe as a zustand selector).
 */
/**
 * 這台主機上「可以拿來啟動 bot」的身份：config.toml 的 `[[identities]]`，加上 daemon 從那台
 * 主機的登入 shell 認出來的 `ccN` alias（SPEC §16）。同名時 config 優先——daemon 那邊
 * (`tools::identities_for_host`) 用的是同一條規則。
 *
 * 是純函式而不是 selector：兩個輸入都是 store 裡的穩定引用，元件端用 `useMemo` 合併，
 * 避免每次 render 都回一個新陣列（React #185）。
 */
export function identitiesOfHost(all: Identity[], status: IdentityStatusMap): Identity[] {
  const out = all.slice()
  for (const st of Object.values(status)) {
    if (st.source !== 'shell' || out.some((i) => i.name === st.name)) continue
    out.push({
      name: st.name,
      kind: st.kind,
      env: st.config_dir ? { CLAUDE_CONFIG_DIR: st.config_dir } : {},
      args: [],
    })
  }
  return out
}

export function identityStatusOfHost(state: StoreState, host: string): IdentityStatusMap {
  if (!host || host === 'local') return state.localIdentityStatus
  return state.hosts.find((h) => h.name === host)?.identity_status ?? NO_IDENTITY_STATUS
}

/** v4.0: `[host, kind]` pairs where the CLI is reported missing (local first). */
export function missingTools(state: StoreState): { host: string; kind: BotKind }[] {
  const out: { host: string; kind: BotKind }[] = []
  const scan = (host: string, tools: ToolMap) => {
    for (const k of BOT_KINDS) if (!tools[k].installed) out.push({ host, kind: k })
  }
  scan('local', state.localTools)
  for (const h of state.hosts) if (h.connected) scan(h.name, h.tools)
  return out
}

/** v4.0: running bots on a host — candidates for `installTool`. */
export function runningBotsOnHost(state: StoreState, host: string): Bot[] {
  return state.bots.filter((b) => {
    if (projectHostName(state, b.project_id) !== (host || 'local')) return false
    const run = state.runs[b.id]
    return run !== null && run !== undefined && run.state === 'running'
  })
}

/** v4.0: the `herdr …` attach command for the host a project sits on. */
export function attachCommandOf(state: StoreState, projectId: string | null): string {
  const host = projectHostName(state, projectId)
  if (host === 'local') return state.attachCommand
  return state.hosts.find((h) => h.name === host)?.attach_command ?? state.attachCommand
}

/**
 * The partial reply to show as a live bubble: only for the turn that is actually in flight.
 *
 * A frame counts as showable when it carries body text, an `activity` row, or an `alert` — a
 * turn that is still only thinking has no text at all, and dropping it here is exactly what used
 * to pin the bubble on "等待回覆（hook）…" for the whole thinking phase.
 */
export function liveReplyOf(state: StoreState, botId: string): LiveReply | null {
  const live = state.liveReply[botId]
  if (!live || (!live.text.trim() && !live.activity.trim() && !live.alert.trim())) return null
  const inflight = inFlightTurn(state, botId)
  return inflight && inflight.id === live.turnId ? live : null
}

/** SPEC §13.5 composer rule: the group composer is open as long as *one* member can be sent to. */
export interface GroupComposerState {
  disabled: boolean
  reason: string
  /** member bots that would accept a prompt right now */
  sendable: string[]
}

// ---------------------------------------------------- SPEC-team selectors

/** 某個 project 底下的 team（建立時間新的排前面）。 */
export function teamsOfProject(state: { teams: Record<string, Team> }, projectId: string): Team[] {
  return Object.values(state.teams)
    .filter((t) => t.project_id === projectId)
    .sort((a, b) => b.created_at.localeCompare(a.created_at) || b.id.localeCompare(a.id))
}

/**
 * SPEC-team §11.3 時間軸：`GET /projects/:id/messages` 的同一份資料，過濾 `team_id`。
 * 專案的群組歷史還沒載入時回 null，讓面板顯示載入狀態而不是「空的」。
 */
export function teamMessages(state: StoreState, teamId: string | null): GroupMessage[] | null {
  if (!teamId) return null
  const projectId = state.teams[teamId]?.project_id ?? state.teamDetail[teamId]?.project_id ?? null
  if (!projectId) return null
  const all = state.groupMessages[projectId]
  if (!all) return null
  return all.filter((m) => m.team_id === teamId)
}

/** team 成員的 Bot 物件，依 pm → worker → reviewer 排序（缺席的成員略過）。 */
export function teamMemberBots(state: StoreState, teamId: string | null): Bot[] {
  const team = teamId ? state.teams[teamId] : null
  if (!team) return []
  const rank = { pm: 0, worker: 1, reviewer: 2 }
  return team.members
    .map((m) => state.bots.find((b) => b.id === m.bot_id))
    .filter((b): b is Bot => Boolean(b))
    .sort((a, b) => rank[a.team?.role ?? 'worker'] - rank[b.team?.role ?? 'worker'] || a.name.localeCompare(b.name))
}

/**
 * SPEC-team §7.3 的短名。定義在 `api/types.ts`（純模組，`teamPanelLogic` 這種可單獨跑
 * `node --test` 的檔案才能用），這裡照舊 re-export，元件的 import 路徑不變。
 */
export { teamShortName } from '../api/types'

export function groupComposerState(state: StoreState, projectId: string | null): GroupComposerState {
  if (!projectId) return { disabled: true, reason: '', sendable: [] }
  const members = state.bots.filter((b) => b.project_id === projectId)
  if (members.length === 0) return { disabled: true, reason: '這個 Project 還沒有 Bot', sendable: [] }
  const sendable = members
    .filter((b) => {
      const cs = composerState(state, b.id)
      return !cs.disabled && !cs.queued
    })
    .map((b) => b.id)
  if (sendable.length === 0) {
    const first = composerState(state, members[0].id).reason
    return { disabled: true, reason: `專案內沒有可送訊息的 Bot（${first}）`, sendable }
  }
  return { disabled: false, reason: '', sendable }
}
