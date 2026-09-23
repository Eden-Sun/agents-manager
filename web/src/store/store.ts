/** Single Zustand store: server state from `GET /api/state` + `/ws`, plus UI state. Event flow / resync: SPEC §7.3. */

import { gatewayErrText, networkErrText } from '../lib/netErr'
import { draftsClearedElsewhere } from './draftSync'
import { createResyncRunner } from './resyncQueue'
import { createRequestId, settleCreateRequest } from '../lib/createRequestId'
import { groupSendDelivered } from './groupSend'
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
  toHerdrVersion,
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
  arr,
} from '../api/normalize'
import { ApiError } from '../api/types'
import { PREVIEW_OFF, toPreviewEvent, type Preview } from '../api/preview'
import type { Bot, BotKind, RestartBatch, GroupChatResult, Mission, MissionDetail, NewMissionInput, MemSnapshot, GroupMessage, Host, HerdrVersion, HostResult, HostShell, Identity, IdentityStatusMap, Lamp, Message, ModelInfo, NewBotInput, NewHostInput, NewIdentityInput, NewProjectInput, PatchBotInput, PatchProjectInput, Project, QuotaMap, Run, TerminalSource, ToolMap, Turn, TurnDelivery } from '../api/types'
import type { ProjectPane } from '../api'
import { joinRunningBatch, restartProgress } from './restartBatch'
import { dropHostModels, modelsKey, shouldFetchModels, type ModelsCache } from './modelsCache'
import { MESSAGE_CAP, byId, byTime, capList, insertSorted, pruneTurns } from './lists'
import { acceptStateSeq, singleFlight } from './singleFlight'
import { quotaForIdentity } from './quotaLookup'
import { botStatusConnTarget } from './botStatusConn'
import { paneReadOnly } from '../lib/shellAccess'
import { groupByProject, withPane, withoutPane } from '../lib/paneLists'
import { prependDraft, restoreQueued } from './queuedSend'
import { noteQueuedTurn, startingSend, startingSendLabel } from './startingSend'
import { asUncommittedSend, noteInFlightTurn, uncommittedSendText } from './uncommittedSend'
import { sendNowFellThrough } from './sendNowOutcome'
import { missionRequests } from './missionRequests'
import { MISSION_USER_PAUSE } from '../lib/missionView'

import type { QueuedSend, RestoreResult } from './queuedSend'
import { laterMark, serverUnread } from './sharedUnread'
import { confirmGroupTurn, dropLegacyGroupCounts, noteGroupPrompt, noteGroupPrompts } from './groupUnread'
import {
  botKey,
  clearHookCompletion,
  completesTurn,
  completionKey,
  countUnreadTurns,
  groupKey,
  idleEdgeCompletionKey,
  loadCounts,
  loadMarks,
  markHookCompletion,
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
import { fetchSupervisor } from '../api/supervisor'
import { IN_MOBILE_PREVIEW, writeShared } from './mobilePreview'
import { BOT_KINDS, LOCAL_HOST } from '../api/types'

const sendMissionRequest = missionRequests(api.newClientRequestId)
const missionLoads = new Map<string, () => Promise<void>>()
const missionListLoads = new Map<string, () => Promise<void>>()
/**
 * 已結案（done／cancelled）各抓最近幾筆；進行中的另外抓、不跟它們搶名額。
 * 以前一次 `status=all&limit=50`：專案又開了 50 筆任務之後，停著等你回答的那筆就掉出清單，卡片跟回答框一起消失（review3 c1 M6）。
 */
export const MISSION_CLOSED_LIMIT = 50
/** 進行中的實際上不設限：daemon 的 `limit` 最多收 500，同一個專案不會同時開著這麼多筆。 */
const MISSION_OPEN_LIMIT = 500

export type SocketStatus = 'connecting' | 'open' | 'closed'
export type RightTab = 'chat' | 'terminal' | 'preview'

export type SettingsAnchor = { left: number; right: number; top: number; bottom: number }

/** 不直接存 DOMRect：會帶著一堆用不到的欄位。 */
export function anchorOf(el: Element): SettingsAnchor {
  const r = el.getBoundingClientRect()
  return { left: r.left, right: r.right, top: r.top, bottom: r.bottom }
}
export type KindDisplay = 'icon' | 'text'

const KIND_DISPLAY_KEY = 'am.kindDisplay'
const DRAFTS_KEY = 'am.drafts'
const DRAFT_CURSORS_KEY = 'am.draftCursors'
const SELECTION_KEY = 'am.selection'

/** Open conversation (mirrored to localStorage). `projectId` non-null = group chat (SPEC §13); `botId` stays parked. */
interface Selection {
  botId: string | null
  projectId: string | null
}

const NO_SELECTION: Selection = { botId: null, projectId: null }

function readSelection(): Selection {
  try {
    const raw = localStorage.getItem(SELECTION_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!isRec(parsed)) return NO_SELECTION
    return {
      botId: optStr(pick(parsed, 'botId')),
      projectId: optStr(pick(parsed, 'projectId')),
    }
  } catch {
    return NO_SELECTION
  }
}

function writeSelection(sel: Selection) {
  try {
    writeShared(() => localStorage.setItem(SELECTION_KEY, JSON.stringify(sel)))
  } catch {
    /* storage unavailable: the selection still holds for this page */
  }
}

const initialSelection = readSelection()

const SHELL_VIEW_KEY = 'am.shellView'

type ShellView = {
  host: string
  paneId: string
  cwd: string
  /** 只能看的 pane（有 listen port 的 dev server 之類）：送一個 Ctrl-C 就是把它關掉（§6.5e）。 */
  readOnly?: boolean
  /** daemon 回 403 時它給的說明；只在這一頁有效，重整不帶回來（`readShellView` 不讀它）。 */
  readOnlyReason?: string
  /** 從選單點進來的 pane，不是這個面板自己開的：關它走 pane 的生命週期，面板不給「結束 shell」。 */
  traced?: boolean
}

/** 重整要回到同一個 shell；pane 可能已關，`bootstrap` 會對 `GET /api/hosts/:host/shells` 驗一次。 */
function readShellView(): ShellView | null {
  try {
    const raw = localStorage.getItem(SHELL_VIEW_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (!isRec(parsed)) return null
    const host = optStr(pick(parsed, 'host'))
    const paneId = optStr(pick(parsed, 'paneId'))
    if (!host || !paneId) return null
    return {
      host,
      paneId,
      cwd: optStr(pick(parsed, 'cwd')) ?? '',
      // 重整之後唯讀要跟著回來，不然 dev server 的面板一重整就又能打字了。
      readOnly: parsed.readOnly === true,
      traced: parsed.traced === true,
    }
  } catch {
    return null
  }
}

function writeShellView(v: ShellView | null) {
  try {
    writeShared(() => (v ? localStorage.setItem(SHELL_VIEW_KEY, JSON.stringify(v)) : localStorage.removeItem(SHELL_VIEW_KEY)))
  } catch {
    /* storage unavailable */
  }
}

const initialShellView = readShellView()

/** Composer drafts survive bot / group / tab switches and reloads. */
export type DraftKey = `bot:${string}` | `group:${string}`

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

/**
 * 多分頁：整份覆寫會把別的分頁剛存的草稿洗掉（分頁 A 打 bot1、分頁 B 的記憶體沒有它，B 一打 bot2 就寫回不含 bot1 的整份）。
 * 只把「這次相對 prev 有變的鍵」套到磁碟上現有的那份（#300）。
 */
function persistDiff<V>(storageKey: string, prev: Record<string, V>, next: Record<string, V>, read: () => Record<string, V>) {
  const disk = read()
  for (const k of Object.keys(prev)) if (!(k in next)) delete disk[k]
  for (const [k, v] of Object.entries(next)) if (prev[k] !== v) disk[k] = v
  localStorage.setItem(storageKey, JSON.stringify(disk))
}

function writeDrafts(prev: Record<string, string>, drafts: Record<string, string>) {
  try {
    writeShared(() => persistDiff(DRAFTS_KEY, prev, drafts, readDrafts))
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

function writeDraftCursors(prev: Record<string, DraftCursor>, cursors: Record<string, DraftCursor>) {
  try {
    writeShared(() => persistDiff(DRAFT_CURSORS_KEY, prev, cursors, readDraftCursors))
  } catch {
    /* storage unavailable: cursors still live for this page */
  }
}

function readKindDisplay(): KindDisplay {
  try {
    return localStorage.getItem(KIND_DISPLAY_KEY) === 'text' ? 'text' : 'icon'
  } catch {
    return 'icon'
  }
}

/** 未讀見 `store/unread.ts`。已讀標記留模組層不進 state：沒畫面讀它，進 state 只多一次 render。 */
const initialUnread = dropLegacyGroupCounts(loadCounts())
let readMarks = loadMarks()

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
  /** 可按的動作（刪 bot 後的「復原」）：放通知上，誤刪當下人就在看它。 */
  action?: { label: string; run: () => void | Promise<void> }
}

/** `turn_progress` 合併間隔（毫秒）：同一個 bot 在這段時間內只套用最後一個 frame。 */
const LIVE_THROTTLE_MS = 250
const liveThrottle = new Map<string, { apply: null | (() => void) }>()

/** issue #25：「載入更早的訊息」一次補幾則（與第一頁同大小）。 */
const PAGE_SIZE = 200

/** WS `turn_progress` (API.md v3.9): the partial reply of an in-flight turn. */
export interface LiveReply {
  turnId: string
  text: string
  /** Pane spinner row (`turn_progress.activity`, API.md v4.1); plain text, never stored; shown only while `text` is empty. `''` = none. */
  activity: string
  /** Retry / API-error banner (`turn_progress.alert`, API.md v4.2): spinner still spins, so UI would look healthy otherwise. `''` = none. */
  alert: string
  revision: number
}

export type { QueuedSend } from './queuedSend'

export interface ComposerState {
  /** 完全不能輸入（未啟動、blocked、主機斷線、送達狀態未知…）。 */
  disabled: boolean
  reason: string
  /** 回合還在跑：送出會排進佇列（daemon 一個 run 只允許一個 in-flight turn，直接送會 409）。 */
  queued: boolean
  inFlightTurnId: string | null
  unknownTurnId: string | null
  /** Bot 沒在跑：送出＝交給 daemon 先收下、它自己啟動 bot，起來後再送（2026-09-18 使用者：不要先擋著；issue #122）。 */
  autoStart?: boolean
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
  /** `GET /api/state` failed; the sidebar may be behind until the next successful refresh. */
  stateStale: boolean

  /** SPEC §11.6 remote hosts; the local machine is never in this list. */
  hosts: Host[]
  attachCommand: string
  /** Local machine's `hosts[0].tools`. */
  localTools: ToolMap
  localHerdr: HerdrVersion
  /** Local machine's `hosts[0].identities`. */
  localIdentityStatus: IdentityStatusMap
  /** 被標為停用的身份，鍵是 `api.identityPrefKey(host, kind, name)`（daemon 端的設定，不是瀏覽器記的）。 */
  disabledIdentities: string[]
  /** SPEC §15；null = 還沒讀到。 */
  mem: MemSnapshot | null
  /** key = kind or `kind:identity`. */
  quota: QuotaMap
  /** keyed `kind@host@identity`; null = fetch failed (use static list). */
  models: ModelsCache
  /** issue #26: last failed fetch time; retried after `MODELS_RETRY_MS`. */
  modelsFailedAt: Record<string, number>
  dropHostModels: (host: string) => void
  kindDisplay: KindDisplay
  /** The "host is missing <kind>" banner, closed for this page load. */
  toolHintDismissed: boolean
  drafts: Record<string, string>
  draftCursors: Record<string, DraftCursor>
  identities: Identity[]
  projects: Project[]
  bots: Bot[]
  runs: Record<string, Run | null>
  turns: Record<string, Record<string, Turn>>
  messages: Record<string, Message[]>
  loadedBots: Record<string, boolean>
  /** issue #25：前面還有更早的（`has_more` 或被 `MESSAGE_CAP` 截掉）；key 是 bot id 或 project id。 */
  moreMessages: Record<string, boolean>
  loadingMore: Record<string, boolean>
  /** bot id → 已完成未讀回合數；純前端帳，存 localStorage（`store/unread.ts`）。 */
  botUnread: Record<string, number>
  /**
   * 側欄目前收起來的 bot（額度停用，見 `store/quotaHide.ts`）。排序過，寫入端只有 quotaHide；
   * 放在這裡是為了讓鍵盤導覽、晶片列與分頁標題跟側欄看到同一批 bot。
   */
  hiddenBotIds: string[]
  /** Render only while `composerState(...).inFlightTurnId === liveReply.turnId` so a stale entry never shows. */
  liveReply: Record<string, LiveReply>

  /** 回合進行中按送出的訊息（每 bot 最多一則），回合結束自動送出。 */
  queuedSends: Record<string, QueuedSend>

  /** SPEC §13 group view; non-null overrides `selectedBotId` (which is kept). */
  selectedProjectId: string | null
  groupMessages: Record<string, GroupMessage[]>
  loadedProjects: Record<string, boolean>
  /** §13.6: replies that arrived while that project's group view was not open (memory only). */
  groupUnread: Record<string, number>

  /**
   * 總管（AGM）專案 id（`GET /api/supervisor`）。排除總管與其工人**只能靠 id**：
   * 專案名可被使用者改（2026-09-13 已改成 `AGM-DM-GRUP`）。`null` = 未讀到／無總管。
   */
  supervisorProjectId: string | null
  loadSupervisorProject: () => Promise<void>

  // 群組任務（mission，docs/goals/agm-missions.md §5）
  missions: Record<string, Mission[]>
  missionDetail: Record<string, MissionDetail>
  missionLoading: Record<string, boolean>
  missionLoadErrors: Record<string, string>
  /** false = daemon 沒有 `/api/missions`（舊版）：入口靜默消失。 */
  missionsSupported: boolean
  /** 已結案那兩段是不是被 `MISSION_CLOSED_LIMIT` 截掉了（標題要說「最近 N 筆」，不能冒充總數）。 */
  missionsCapped: Record<string, { done: boolean; cancelled: boolean }>
  loadMissions: (projectId: string) => Promise<void>
  /** 重連／resync 之後把已經載過的清單與任務重抓一次：斷線期間的 `mission_updated` 收不到。 */
  refreshLoadedMissions: () => Promise<void>
  loadMission: (missionId: string) => Promise<void>
  startMission: (projectId: string, input: NewMissionInput) => Promise<string | null>
  controlMission: (missionId: string, action: 'pause' | 'resume' | 'cancel') => Promise<void>
  answerMission: (missionId: string, text: string) => Promise<boolean>
  /** 只留話並叫醒 AGM，不改任何交付。 */
  askMission: (missionId: string, text: string) => Promise<boolean>
  /** 開一筆續作，原成果不會被改到。 */
  reviseMission: (missionId: string, text: string) => Promise<string | null>

  /** 非 null = 主面板顯示 `HostShellPanel`（優先於其他選取）。 */
  shellView: ShellView | null

  selectedBotId: string | null
  rightTab: RightTab
  /** 預覽模式（issue #253）：每顆頂層 bot 的 dev server 預覽；`preview_changed` 與面板的 GET／POST／DELETE 寫入。 */
  previews: Record<string, Preview>
  setPreview: (botId: string, p: Preview) => void
  settingsBotId: string | null
  /** null = 置中。 */
  settingsAnchor: SettingsAnchor | null
  /** 樂觀順序（key = project id），只是等 `project_changed` 的過渡；權威順序是 daemon 的陣列。 */
  botOrder: Record<string, string[]>
  projectOrder: string[]
  openBotSheetFor: string | null
  notices: Notice[]
  busy: Record<string, boolean>

  bootstrap: () => Promise<void>
  refreshState: () => Promise<void>
  selectBot: (botId: string | null) => void
  /** 以側邊欄順序往前 / 後選一個（頭尾繞回去）。 */
  selectAdjacentBot: (dir: -1 | 1) => void
  selectProject: (projectId: string | null) => void
  loadGroupMessages: (projectId: string) => Promise<void>
  loadEarlierGroupMessages: (projectId: string) => Promise<void>
  /** null = failed (reason already shown as a notice). */
  sendGroupChat: (projectId: string, text: string, attachments?: string[]) => Promise<GroupChatResult | null>
  setRightTab: (tab: RightTab) => void
  openSettings: (botId: string, anchor?: SettingsAnchor | null) => void
  /** `beforeId = null` = 移到最後。同專案內才有效。 */
  moveBot: (botId: string, beforeId: string | null) => void
  /** 主力那列（★）的固定順序（#344）：整份順序（陣列位置＝primary_position）；樂觀套用、失敗回捲並提示。 */
  movePrimary: (order: string[]) => void
  moveProject: (projectId: string, beforeId: string | null) => void
  closeSettings: () => void
  requestOpenBotSheet: (projectId: string) => void
  clearOpenBotSheet: () => void
  loadMessages: (botId: string) => Promise<void>
  loadEarlierMessages: (botId: string) => Promise<void>
  markBotRead: (botId: string) => void
  markGroupRead: (projectId: string) => void
  markCurrentRead: () => void
  /** 存下來的數字只是重整前的快照，訊息載入後要用已讀標記重算。 */
  recountBot: (botId: string) => void
  pruneUnread: () => void
  notify: (kind: Notice['kind'], text: string, action?: Notice['action']) => void
  dismiss: (id: number) => void

  startBot: (botId: string) => Promise<void>
  stopBot: (botId: string) => Promise<void>
  interruptBot: (botId: string) => Promise<void>
  /** 強制結束目前回合（送不送得出 `esc` 都解鎖）。 */
  abortBot: (botId: string) => Promise<boolean>
  /** 送 `/login` 進 TUI；false = 沒送出，原因已跳通知。 */
  loginBot: (botId: string) => Promise<boolean>
  /** `attachments` 是 `POST /bots/:id/attachments` 回傳的 id。 */
  /** `sendNow`＝插隊送出（issue #103）：對方回合中時打斷它，而不是排隊／409。 */
  /** `startIfStopped`：bot 沒在跑時交給 daemon 先收下再啟動（issue #122），不經瀏覽器佇列。 */
  sendPrompt: (botId: string, text: string, attachments?: string[], sendNow?: boolean, startIfStopped?: boolean) => Promise<boolean>
  sendKeys: (botId: string, keys: string[]) => Promise<void>
  /** 多行內容要走這裡：`sendKeys` 吃鍵名，`\n` 不是鍵名（見 `store/alongside.ts`）。 */
  sendText: (botId: string, text: string, enter: boolean) => Promise<boolean>
  abandonTurn: (botId: string, turnId: string) => Promise<void>
  addHost: (input: NewHostInput) => Promise<HostResult | null>
  addIdentity: (input: NewIdentityInput) => Promise<boolean>
  removeIdentity: (name: string, host?: string) => Promise<void>
  removeHost: (name: string) => Promise<void>
  reconnectHost: (name: string) => Promise<HostResult | null>
  addProject: (input: NewProjectInput) => Promise<boolean>
  addBot: (projectId: string, input: NewBotInput) => Promise<string | null>
  cloneBot: (botId: string) => Promise<string | null>
  /** 分出新 bot 並接續來源的對話脈絡（daemon 建好就啟動）。null = 失敗（原因已跳通知）。 */
  forkBot: (botId: string) => Promise<string | null>
  /** 子 agent 升級成頂層 bot（保留對話）。回新 bot id；null = 失敗（原因已跳通知）。 */
  promoteBot: (botId: string) => Promise<string | null>
  /** 回傳 `needs_restart`；null = 失敗（原因已跳通知）。 */
  patchBot: (botId: string, input: PatchBotInput) => Promise<boolean | null>
  patchProject: (projectId: string, input: PatchProjectInput) => Promise<boolean>
  /** `resumeNative`：換身分後重啟要接回原對話（`?resume=native`），接不回自動退回不帶旗標重送一次。 */
  restartBot: (botId: string, resumeNative?: boolean) => Promise<boolean>
  /** SPEC §6.9：閒置的 claude bot 全部 exit + resume；忙的跳過。 */
  restartIdleBots: () => Promise<void>
  /** null = 沒有批次在跑，也沒有摘要要看。 */
  restartBatch: RestartBatch | null
  clearRestartBatch: () => void
  removeBot: (botId: string) => Promise<void>
  removeProject: (projectId: string) => Promise<void>
  readTerminal: (botId: string, source: TerminalSource, lines: number) => ReturnType<typeof api.fetchTerminal>

  /** 已有活著的 shell 就接回最新那個，不再多開。false = 沒開起來（原因已跳通知）。 */
  openHostShell: (host: string, cwd?: string) => Promise<boolean>
  /** 切到已開著的 shell，不打 API。 */
  viewHostShell: (shell: HostShell) => void
  /** SPEC §6.5e：每個專案被 trace 的 pane（依 `project_id` 分）。側欄與專案頁讀的是同一份。 */
  sidePanes: Record<string, ProjectPane[]>
  /** 對不到專案的 pane：側欄底部「開 shell」旁那一組，不掛在任何專案底下。 */
  unownedPanes: ProjectPane[]
  /** 重讀上面兩份（single-flight）；舊 daemon 沒有端點時什麼都不動。 */
  refreshPanes: () => Promise<void>
  /** 專案頁的「關閉」。`'closed'`＝不在了；回一列 pane＝服務 pane 要先確認（daemon 409 附上的最新那列）；`null`＝失敗（已通知）。 */
  closeTracedPane: (pane: ProjectPane, confirm: boolean) => Promise<'closed' | ProjectPane | null>
  /** 面板讀到 404：那顆已經不在了。收掉面板、兩份清單一起拿掉，講一聲。 */
  paneGone: (host: string, paneId: string) => void
  viewPane: (pane: ProjectPane) => void
  /** 只關面板，shell 留著。 */
  closeShellView: () => void
  /** daemon 回 403（只能看／正在跑 agent）：面板鎖成唯讀並顯示它的說明。 */
  lockShellView: (host: string, paneId: string, reason: string) => void
  restoreShellView: () => Promise<void>
  /**
   * 結束 shell。`confirm` 只在人看過「正在 listen」或「讀不到狀態」之後才帶；沒帶而 daemon 要確認時，
   * 回傳它附上的那一列（面板據此再問一次），**不關也不跳錯誤**。
   */
  endHostShell: (host: string, paneId: string, confirm?: boolean) => Promise<api.CloseNeedsConfirm | null>

  loadQuota: () => Promise<void>
  loadMem: () => Promise<void>
  loadModels: (kind: BotKind, host: string, identity?: string | null) => Promise<ModelInfo[] | null>
  /** Ask a running bot on `host` to install + log in `kind`; opens that bot's chat. null = failed. */
  installTool: (host: string, kind: BotKind, viaBotId: string) => Promise<string | null>
  loginIdentity: (host: string, identity: string) => Promise<boolean>
  logoutIdentity: (host: string, identity: string) => Promise<boolean>
  loadIdentityPrefs: () => Promise<void>
  setIdentityDisabled: (host: string, kind: string, name: string, disabled: boolean) => Promise<void>
  /** `''` / `local` = this machine. */
  refreshTools: (host: string) => Promise<boolean>
  setKindDisplay: (mode: KindDisplay) => void
  dismissToolHint: () => void
  /** Empty text removes the draft. */
  setDraft: (key: DraftKey, text: string) => void
  /** Positions are clamped to the current draft text. */
  setDraftCursor: (key: DraftKey, start: number, end?: number) => void

  queueSend: (botId: string, text: string, attachments: string[]) => void
  cancelQueuedSend: (botId: string) => void
  /** 取消排隊：那則接回輸入框最前面（不蓋掉正在打的字）。 */
  unqueueToDraft: (botId: string) => void
  /** 送失敗時放回佇列或輸入框；三個送出入口共用，免得 409 把字吃掉。 */
  restoreQueuedSend: (botId: string, pending: QueuedSend) => void
  /** issue #122：撤回 daemon 那一則「等 bot 起來」的訊息，文字接回輸入框最前面。 */
  cancelStartingSend: (botId: string) => Promise<void>
}

let noticeSeq = 0

/**
 * fork 的錯誤說人話。405／404（不是 `bot` 找不到）＝daemon 還沒有這支 API：開發用的前端常常比跑著的 daemon 新
 * （2026-09-15 使用者按了「接續對話」卻只看到 Method Not Allowed）。409 帶的 `message` 比 `reason` 代碼好懂。
 */
function forkErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 405 || (e.status === 404 && e.body.what !== 'bot')) {
      return 'fork 失敗：正在跑的 daemon 還沒有 fork 功能，要重新 build 並重啟 daemon 後才能用。'
    }
    if (typeof e.body.message === 'string' && e.body.message) return `fork 失敗：${e.body.message}`
  }
  return `fork 失敗：${errText(e)}`
}

const PROMOTE_REASON_TEXT: Record<string, string> = {
  session_not_found: '找不到它的 claude session（pane 裡沒有對得上的行程或對話檔）',
  session_ambiguous: 'pane 裡有不只一段 claude session，無法確定要接哪一段',
  transcript_exists: '目標目錄已經有一份不同內容的同名對話檔，不覆寫',
  has_children: '它自己還有子 agent，先處理掉再升級',
  stop_failed: '停不掉這顆子 agent，什麼都沒動',
  remote_not_supported: '遠端主機上的子 agent 還不支援升級',
  unsupported_kind: '只有 claude 的子 agent 能升級',
  promote_start_failed: '升級後的新 bot 起不來，已收回',
}

function promoteErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 405 || (e.status === 404 && e.body.what !== 'bot')) {
      return '升級失敗：正在跑的 daemon 還沒有這個功能，要重新 build 並重啟 daemon 後才能用。'
    }
    const why = typeof e.body.reason === 'string' ? PROMOTE_REASON_TEXT[e.body.reason] : undefined
    if (why) return `升級失敗：${why}`
    if (typeof e.body.message === 'string' && e.body.message) return `升級失敗：${e.body.message}`
  }
  return `升級失敗：${errText(e)}`
}

/** daemon 回的機器 key 換成人話；沒列到的照原樣顯示。 */
const REASON_TEXT: Record<string, string> = {
  composer_unreadable: '沒送出：讀不到 bot 的輸入框（可能正在切換畫面或還在啟動），稍後再送一次',
  // 這幾個是 daemon 打字送出前就擋下的 409（`sent:false`、不留 turn）：字還在輸入框，稍後原樣重送。
  composer_busy: '沒送出：bot 的輸入框裡有字（可能有人正在終端打字，或上一次插隊沒送出的字還留著），清掉或送出之後再送一次',
  transcript_not_ready: '沒送出：claude 還沒回報這段對話（session），稍後再送一次',
  transcript_unreadable: '沒送出：讀不到這段對話的紀錄檔，稍後再送一次',
  codex_log_not_ready: '沒送出：codex 的紀錄還沒寫出來，稍後再送一次',
  no_pane_to_type_into: '沒送出：找不到這顆 bot 的終端畫面可以打字，重啟它再試',
  resume_unverified: '沒送出：這顆 bot 是接回舊對話起來的，還在確認接回的是不是原本那段（最多約兩分鐘），稍後再送一次',
  // API.md 有列、daemon 不附 `message`：讀不到這顆 bot 在哪台主機（daemon 的資料暫時讀不到），字沒打進去（#233）。
  host_unreadable: '沒送出：讀不到這顆 bot 在哪台主機（daemon 的資料暫時讀不到），稍後再送一次',
}

/**
 * daemon 的 409 帶機器 key `reason` 時，常常另外附一句人話 `message`（維護窗口、Esc 送出去了不知道進了沒有、
 * 登入指令找不到 CLI、default session 的 bot 不給開關…）；`ApiError.message` 取的是 `reason` 代碼，直接顯示只剩
 * `maintenance_window（HTTP 409）`。以前只有 `retryable:true` 才用 `message`，不可重試的那幾條照樣只剩代碼（#233）。
 * `reason`（機器 key）與 `message`（人話）是分開的兩個欄位（API.md），兩個都在就顯示人話。
 */
function reasonText(e: ApiError): string {
  const known = REASON_TEXT[e.message]
  if (known) return known
  const human = e.body.message
  if (typeof e.body.reason === 'string' && typeof human === 'string' && human.trim()) return human.trim()
  return e.message
}

/**
 * 送鍵／送字帶的 `expect_run_id` 是這一顆 bot 被重啟過的圍籬：run 換了就不該把鍵打進新的 agent。
 *
 * 但前端快取的 run id 會過期（bot 剛重啟、狀態還沒推到；或 daemon 換版後第一次操作），使用者看到的
 * 只是「送出按鍵失敗：run mismatch（HTTP 409）」，得自己重按一次——2026-09-19 w168:p7J 就是這樣。
 * daemon 的 409 本來就把**現在的** run id 放在 body 裡，所以這裡拿它重試一次；再失敗才報錯。
 * 只對 `run mismatch` 重試，而且只重試一次：其他 409（框裡有字、回合在飛）照舊原樣回報。
 */
export async function sendWithFreshRun(get: () => StoreState, botId: string, send: (runId: string | null) => Promise<unknown>): Promise<void> {
  try {
    await send(get().runs[botId]?.id ?? null)
  } catch (e) {
    const fresh = e instanceof ApiError && e.status === 409 && e.message === 'run mismatch' ? e.body.run_id : undefined
    if (typeof fresh !== 'string' || !fresh) throw e
    await send(fresh)
    // 快取已經過期，順手把狀態拉回來（失敗不影響這次送出）。
    void get().refreshState()
  }
}

function errText(e: unknown): string {
  const net = networkErrText(e)
  if (net) return net
  if (e instanceof ApiError) {
    const human = typeof e.body.message === 'string' && e.body.message.trim() !== ''
    const gw = gatewayErrText(e.status, human || typeof e.body.error === 'string')
    if (gw) return gw
    return `${reasonText(e)}（HTTP ${e.status}）`
  }
  if (e instanceof Error) return e.message
  return String(e)
}

/**
 * 明確指定的 identity 確認沒登入（GH #83）：daemon 的 409 已經把身分／主機／怎麼登入寫成人話放在
 * `hint`，直接顯示它，不要漏成原始的 `identity_not_logged_in` 代碼。
 */
function startErrText(e: unknown): string {
  if (e instanceof ApiError && e.body.reason === 'identity_not_logged_in' && typeof e.body.hint === 'string' && e.body.hint) {
    return e.body.hint
  }
  return errText(e)
}

/** API.md：`start_state_uncommitted`／`stop_state_uncommitted`／`restart_state_uncommitted`——外面做了、DB 還沒寫成。 */
function isRunStateUncommitted(e: unknown, which: 'start' | 'stop' | 'restart'): boolean {
  return e instanceof ApiError && e.status === 503 && e.body.error === `${which}_state_uncommitted`
}

function isActiveRunConflict(e: unknown): boolean {
  return e instanceof ApiError && e.status === 409 && e.body.reason === 'active run already exists'
}

function runStateUncommittedText(e: unknown): string {
  if (e instanceof ApiError && typeof e.body.message === 'string' && e.body.message.trim()) return e.body.message.trim()
  return errText(e)
}

/** daemon 回穩定的機器 key，文案留在前端。 */
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
 * 一頁 messages 回來時舊清單留哪些：不能整包換（飛行中收到的 `message_added` 會被蓋掉），
 * 也不能全留（resync 要能刪過期的）。界線是頁內最新一筆，空頁退回 `startedAt`。
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
let supervisorProjectAsked = false
let identityPrefsAsked = false
let lastRefreshError: string | null = null
/**
 * 送不出去的已讀。2026-09-15 改成「未讀數以 daemon 為準」之後，這個 POST 就不再是 fire-and-forget：
 * 失敗代表下一次 `refreshState` 會拿 daemon 的舊數字把本機剛清掉的徽章點回來（剛讀完的 bot 又亮
 * `!3`，而且不再點一次就一直亮著）。補送成功前 `serverUnread` 一律跳過這些 bot。
 */
const unsentReads = new Map<string, ReadMark>()

function sendReadMark(botId: string, mark: ReadMark) {
  unsentReads.set(botId, mark)
  void api
    .markBotRead(botId, mark)
    .then(() => {
      // 只有還是同一筆標記才清：中途又讀過一次的話那筆還沒送到。
      if (unsentReads.get(botId) === mark) unsentReads.delete(botId)
    })
    .catch(() => {})
}

/** socket 重開＝daemon 回來了，把欠的已讀補送出去（別台裝置也才看得到）。 */
function flushUnsentReads() {
  for (const [botId, mark] of [...unsentReads]) sendReadMark(botId, mark)
}

/** 在飛的 `POST /api/order` 數；歸零時 `refreshState` 清掉樂觀順序，別台裝置的順序才會過來。 */
let orderSavesInFlight = 0
/** 每個排序範圍（專案／某專案的 bot／主力）最新一次存檔的代次；晚到的舊失敗不准回滾較新的結果（#275）。 */
const orderSaveGen = new Map<string, number>()
/**
 * 同一排序範圍的存檔排成一條：前一個 POST 結束（不論成敗）才送下一個。每個 POST 都是整份順序、後到的贏，
 * 並行送的話先發的舊順序可能晚到、蓋掉 daemon 已存的新順序，兩邊都 200 所以不會有任何提示（#391）。
 */
const orderSaveTail = new Map<string, Promise<void>>()
function saveOrderTracked(key: string, input: Parameters<typeof api.saveOrder>[0], onFail: (e?: unknown) => void) {
  orderSavesInFlight += 1
  const gen = (orderSaveGen.get(key) ?? 0) + 1
  orderSaveGen.set(key, gen)
  const send = () => api.saveOrder(input)
  const done = (orderSaveTail.get(key) ?? Promise.resolve())
    .then(send, send)
    .catch((e) => {
      if (orderSaveGen.get(key) === gen) onFail(e)
    })
    .finally(() => {
      orderSavesInFlight -= 1
      if (orderSaveTail.get(key) === done) orderSaveTail.delete(key)
    })
  orderSaveTail.set(key, done)
}

/**
 * daemon `seq` 重啟會從 0 重來；不歸零的話新快照全被當舊的丟掉、畫面凍住（2026-09-11 使用者回報）。
 * 所以 socket 重連或收到 `resync` 時歸零。
 */
function resetStateSeq() {
  appliedStateSeq = 0
}

export const useStore = create<StoreState>((set, get) => ({
  ready: false,
  bootError: null,
  socket: 'connecting',
  connected: true,
  defaultConnected: false,
  lastSeq: 0,
  stateStale: false,

  hosts: [],
  attachCommand: 'herdr --session agents-manager',
  localTools: toToolMap(undefined),
  localHerdr: toHerdrVersion(undefined),
  localIdentityStatus: {},
  disabledIdentities: [],
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
  hiddenBotIds: [],
  liveReply: {},
  queuedSends: {},

  selectedProjectId: initialSelection.projectId,
  groupMessages: {},
  loadedProjects: {},
  groupUnread: initialUnread.groups,

  missions: {},
  missionDetail: {},
  missionLoading: {},
  missionLoadErrors: {},
  missionsSupported: true,
  missionsCapped: {},
  shellView: initialShellView,
  supervisorProjectId: null,

  selectedBotId: initialSelection.botId,
  rightTab: 'chat',
  previews: {},
  setPreview: (botId, p) => set((s) => ({ previews: { ...s.previews, [botId]: p } })),
  settingsBotId: null,
  settingsAnchor: null,
  botOrder: {},
  projectOrder: [],
  openBotSheetFor: null,
  notices: [],
  busy: {},
  restartBatch: null,

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
      // `refreshState` 永不 throw；首次失敗就 ready 會讓 routeSync 把深連結當成 Bot 不在、改成 `/`。
      if (get().stateStale) throw new Error(lastRefreshError ?? '無法讀取 daemon 狀態')
      await get().restoreShellView()
      set({ ready: true, bootError: null })
    } catch (e) {
      set({ ready: false, bootError: errText(e) })
      return
    }
    connectSocket(set, get)
    void get().loadQuota()
    void get().loadMem()
    // 安全網：daemon 不重啟也可能在執行中清掉某個額度 key（例如收掉 kind 不符的身分），WS 不會說。
    // 每 5 分鐘整份換一次，最慢 5 分鐘內消失；GET /api/quota 很便宜。
    if (!quotaSweep) quotaSweep = setInterval(() => void get().loadQuota(), QUOTA_SWEEP_MS)
  },

  // issue #23：single-flight + trailing，N 個 frame 只換一、兩次請求；`acceptStateSeq` 丟掉較舊快照。
  refreshState: singleFlight(async () => {
    const st = await api.fetchState()
    const seq = acceptStateSeq(appliedStateSeq, st.daemon_seq)
    if (seq === null) {
      lastRefreshError = null
      set({ stateStale: false })
      return
    }
    // A just-restarted daemon may briefly return an empty snapshot; don't blank the sidebar while the socket is down.
    if (st.projects.length === 0 && get().projects.length > 0 && get().socket !== 'open') return
    lastRefreshError = null
    set({ stateStale: false })
    appliedStateSeq = seq
    // 快照在飛時 WS 已推進到更新的 seq：那些 frame（例如 `bot_status`）先套用過了，這份較舊的快照
    // 會把 run 狀態蓋回舊的。套完補抓一次；daemon 重啟（seq 變小）時下面會把 lastSeq 降下來，只多這一次。
    const behindFrames = get().lastSeq > st.daemon_seq
    const runs: Record<string, Run | null> = {}
    for (const b of st.bots) runs[b.id] = st.runs.find((r) => r.bot_id === b.id) ?? null
    set((s) => {
      const turns = { ...s.turns }
      for (const [botId, map] of Object.entries(turns)) {
        const runId = runs[botId]?.id
        turns[botId] = Object.fromEntries(Object.entries(map).filter(([, t]) => t.status !== 'in_flight' || t.run_id === runId))
      }
      for (const t of st.turns) {
        const botId = t.bot_id ?? st.bots.find((b) => runs[b.id]?.id === t.run_id)?.id
        if (!botId) continue
        if (t.status === 'in_flight' && t.run_id !== runs[botId]?.id) continue
        turns[botId] = pruneTurns({ ...(turns[botId] ?? {}), [t.id]: t })
      }
      const selected =
        s.selectedBotId && st.bots.some((b) => b.id === s.selectedBotId)
          ? s.selectedBotId
          : (st.bots[0]?.id ?? null)
      const selectedProject =
        s.selectedProjectId && st.projects.some((p) => p.id === s.selectedProjectId) ? s.selectedProjectId : null
      // 分身佔位列：同名真 bot 到了就在同一次 set 裡原地替換（含 `botOrder` 位子），清單不跳。
      const keptPending: Bot[] = []
      let botOrder = s.botOrder
      for (const b of s.bots) {
        if (!b.pending) continue
        const real = st.bots.find((r) => r.project_id === b.project_id && r.name === b.name)
        if (!real) {
          keptPending.push(b)
          continue
        }
        const order = (botOrder[b.project_id] ?? []).map((x) => (x === b.id ? real.id : x))
        botOrder = { ...botOrder, [b.project_id]: order }
      }
      // 沒有 order 在飛就讓位給權威順序；只留還有佔位列的專案（佔位要靠它排在原 bot 旁）。
      let projectOrder = s.projectOrder
      if (orderSavesInFlight === 0) {
        projectOrder = []
        const keep: Record<string, string[]> = {}
        for (const b of keptPending) if (botOrder[b.project_id]) keep[b.project_id] = botOrder[b.project_id]
        botOrder = keep
      }
      return {
        hosts: st.hosts,
        attachCommand: st.attach_command,
        localTools: st.tools,
        localHerdr: st.herdr,
        localIdentityStatus: st.identity_status,
        identities: st.identities,
        projects: st.projects,
        bots: [...st.bots, ...keptPending],
        botOrder,
        projectOrder,
        runs,
        turns,
        connected: st.connected,
        defaultConnected: st.default_connected,
        // daemon 重啟後 seq 變小要跟著降，否則 `?since=` 送未來數字會一直 `resync`。
        lastSeq: st.daemon_seq < s.lastSeq ? st.daemon_seq : Math.max(s.lastSeq, st.daemon_seq),
        selectedBotId: selected,
        selectedProjectId: selectedProject,
      }
    })
    {
      const s = get()
      const next = serverUnread(s.bots, s.botUnread, (id) => viewingBot(s, id) && windowActive(), unsentReads)
      if (next) {
        set({ botUnread: next })
        persistUnread(get())
      }
    }
    get().pruneUnread()
    if (behindFrames) void get().refreshState()
    const sel = get().selectedBotId
    if (sel && !get().loadedBots[sel]) await get().loadMessages(sel)
    const proj = get().selectedProjectId
    if (proj && !get().loadedProjects[proj]) await get().loadGroupMessages(proj)
    // 讀不到就算了：排除規則退回「不排除」，只是多幾顆晶片。
    if (get().supervisorProjectId === null) void get().loadSupervisorProject()
    if (!identityPrefsAsked) {
      identityPrefsAsked = true
      void get().loadIdentityPrefs()
    }
  }, (e) => reportStateRefreshError(set, get, e)),

  async loadSupervisorProject() {
    // 同步旗標：開機時 `refreshState` 連跑多輪，只看 state 的 null 會重複送（實測 2 次）。
    if (supervisorProjectAsked) return
    supervisorProjectAsked = true
    try {
      const info = await fetchSupervisor()
      if (info?.project_id) set({ supervisorProjectId: info.project_id })
    } catch {
      // 不值得跳通知；下次重整再試。
      supervisorProjectAsked = false
    }
  },

  selectBot: (botId) => {
    set({ selectedBotId: botId, selectedProjectId: null, shellView: null, rightTab: 'chat', settingsBotId: null })
    // 視窗在前景才算看到（程式化選取可能發生在背景分頁）。
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
      noteGroupPrompts(page.messages)
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
      // Lock each recipient's composer right away (same as `sendPrompt`).
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
      if (!groupSendDelivered(res)) {
        // 一顆都沒送到：回 null，輸入框才會保留草稿與附件（#340）。
        get().notify('error', `一顆都沒送到，草稿已保留：${res.skipped.map((x) => `@${x.bot_name}（${x.detail || x.reason}）`).join('、')}`)
        return null
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

  openSettings: (botId, anchor = null) => {
    // 面板只在 ChatPanel 渲染：不清 shellView 的話按齒輪畫面不動。
    set({
      selectedBotId: botId,
      selectedProjectId: null,
      shellView: null,
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
      const prev = s.botOrder[pid]
      // 順序存 daemon（config.toml）讓各裝置一致；先樂觀套用，等 `project_changed`。
      // 失敗一定要自己收回：daemon 沒收到就不會推 `project_changed`、也就不會 `refreshState`，
      // 「讓位給權威順序」那條路永遠走不到，樂觀順序反而是最持久的——跟通知說的正好相反。
      saveOrderTracked(`bots:${pid}`, { bots: { [pid]: next } }, () => {
        set((st) => ({ botOrder: prev ? { ...st.botOrder, [pid]: prev } : withoutKey(st.botOrder, pid) }))
        get().notify('error', '排序沒存起來（daemon 沒收到），已回到原本的順序')
      })
      return { botOrder }
    })
  },

  movePrimary: (order) => {
    const s = get()
    const prev = new Map(s.bots.map((b) => [b.id, b.primary_position]))
    const pos = new Map(order.map((id, i) => [id, i]))
    if (order.every((id) => prev.get(id) === pos.get(id))) return
    set({ bots: s.bots.map((b) => (pos.has(b.id) ? { ...b, primary_position: pos.get(b.id)! } : b)) })
    saveOrderTracked('primary', { primary: order }, (e) => {
      set((st) => ({ bots: st.bots.map((b) => (prev.has(b.id) && pos.has(b.id) ? { ...b, primary_position: prev.get(b.id)! } : b)) }))
      // 把 daemon 的原因帶出來（舊 daemon 沒有 primary 這個欄位會回 400）：只說「沒收到」使用者看不出為什麼，重整後順序又回去。
      get().notify('error', `主力順序沒存起來，已回到原本的順序：${errText(e)}`)
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
      const prev = s.projectOrder
      saveOrderTracked('projects', { projects: next }, () => {
        set({ projectOrder: prev })
        get().notify('error', '排序沒存起來（daemon 沒收到），已回到原本的順序')
      })
      return { projectOrder: next }
    })
  },

  requestOpenBotSheet: (projectId) => set({ openBotSheetFor: projectId }),
  clearOpenBotSheet: () => set({ openBotSheetFor: null }),

  async loadMessages(botId) {
    try {
      const startedAt = new Date().toISOString()
      const page = await api.fetchMessages(botId)
      noteGroupPrompts(page.messages)
      set((s) => {
        const kept = keptAfterPage(s.messages[botId] ?? [], page.messages, startedAt)
        const turns: Record<string, Turn> = {}
        // 請求飛出後 `sendPrompt` 塞的本地 in_flight turn 是輸入框的鎖，清掉會撞 409；頁裡有的以頁為準。
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
          // 舊頁不灌 `turns`：它只服務 in-flight / unknown 判斷（issue #25 `pruneTurns`）。
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
    const mark = markOfMessages(get().messages[botId] ?? []) ?? markNow()
    setReadMark(botKey(botId), mark)
    if (!botId.startsWith('pending:')) sendReadMark(botId, mark)
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
    if (s.shellView) return
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
    const serverMark = s.bots.find((b) => b.id === botId)?.read_mark
    const n = countUnreadTurns(s.messages[botId] ?? [], laterMark(readMarks[botKey(botId)], serverMark))
    if ((s.botUnread[botId] ?? 0) === n) return
    set((cur) => ({ botUnread: n > 0 ? { ...cur.botUnread, [botId]: n } : withoutKey(cur.botUnread, botId) }))
    persistUnread(get())
  },

  async startBot(botId) {
    await guarded(
      set,
      get,
      `start:${botId}`,
      async () => {
        try {
          await api.startBot(botId)
        } catch (e) {
          // 503：agent 已經起來；409 已有 run：側欄還沒跟上。都不是「沒啟動」。
          if (isRunStateUncommitted(e, 'start')) {
            get().notify('error', runStateUncommittedText(e))
          } else if (!isActiveRunConflict(e)) {
            throw e
          }
        }
        await get().refreshState()
        if (get().queuedSends[botId]) flushQueued(botId)
      },
      startErrText,
    )
  },

  async stopBot(botId) {
    await guarded(set, get, `stop:${botId}`, async () => {
      try {
        await api.stopBot(botId)
      } catch (e) {
        if (isRunStateUncommitted(e, 'stop')) {
          get().notify('error', runStateUncommittedText(e))
        } else {
          throw e
        }
      }
      await get().refreshState()
    })
  },

  async interruptBot(botId) {
    await guarded(set, get, `intr:${botId}`, async () => {
      await api.interruptBot(botId)
    })
  },

  async abortBot(botId) {
    // true 只代表 esc 真的送進終端：「中止並取代」據此決定能否接著送。
    let stopped = false
    await guarded(set, get, `abort:${botId}`, async () => {
      const r = await api.abortBot(botId)
      const n = r.aborted.length
      stopped = r.keys_sent
      get().notify(
        r.keys_sent ? 'info' : 'error',
        r.keys_sent
          ? `已中止 ${n} 個回合`
          : `已中止 ${n} 個回合，但 esc 送不進終端——agent 那邊可能還在跑，必要時停掉 Bot`,
      )
    })
    return stopped
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
    // 槽位只有一格，而 UI 完全沒表達這個上限（輸入框清空、還寫著「先打下一則」）。不先把舊的接回去，
    // 第二次 Enter 會把第一則從 store 裡整個刪掉：沒有通知、沒有草稿，附件 id 也一起孤兒化。
    const prev = get().queuedSends[botId]
    set((st) => ({ queuedSends: { ...st.queuedSends, [botId]: { text, attachments } } }))
    if (!prev) return
    const r = restoreQueued(get(), botId, prev)
    applyRestore(set, get, r)
    const lost = r.droppedAttachments > 0 ? `，${r.droppedAttachments} 個附件要重新加` : ''
    get().notify('error', `一次只排得下一則，前一則已退回輸入框${lost}`)
  },

  cancelQueuedSend(botId) {
    set((st) => ({ queuedSends: withoutKey(st.queuedSends, botId) }))
  },

  unqueueToDraft(botId) {
    const pending = get().queuedSends[botId]
    if (!pending) return
    const key = `bot:${botId}` as const
    // 接在現有草稿前面，不是蓋掉：輸入框裡可能正是上一次被退回的那一則。
    const text = prependDraft(pending.text, get().drafts[key] ?? '')
    set((st) => ({ queuedSends: withoutKey(st.queuedSends, botId) }))
    get().setDraft(key, text)
    get().setDraftCursor(key, pending.text.length)
    if (pending.attachments.length > 0) {
      get().notify('error', `訊息已放回輸入框，但 ${pending.attachments.length} 個附件要重新加`)
    }
  },

  restoreQueuedSend(botId, pending) {
    const r = restoreQueued(get(), botId, pending)
    applyRestore(set, get, r)
    if (r.droppedAttachments > 0) {
      get().notify('error', `訊息已退回輸入框，但 ${r.droppedAttachments} 個附件要重新加`)
    }
  },

  async cancelStartingSend(botId) {
    const pending = startingSend(get().turns[botId], get().messages[botId])
    if (!pending) return
    // 連點：第一下撤回成功之後，第二下拿到 409（那一則已經是 failed）——不能被說成「撤不回來」。
    await guarded(set, get, `withdraw:${botId}`, async () => {
      try {
        await api.withdrawTurn(pending.turnId)
      } catch (e) {
        // 409：已經被佇列送出去了，撤不回來。
        get().notify('error', `撤不回來（可能已經送出）：${errText(e)}`)
        await get().loadMessages(botId)
        return
      }
      const key = `bot:${botId}` as const
      get().setDraft(key, prependDraft(pending.text, get().drafts[key] ?? ''))
      get().setDraftCursor(key, pending.text.length)
      if (pending.attachments > 0) get().notify('error', `訊息已放回輸入框，但 ${pending.attachments} 個附件要重新加`)
      await get().loadMessages(botId)
    })
  },

  async sendPrompt(botId, text, attachments = [], sendNow = false, startIfStopped = false) {
    // 連線斷在 daemon 收下之後（回應遺失）：使用者會再按一次同一句，要拿同一個 crid，daemon 才認得是同一件事而不是再送一次。
    // 只有「沒收到任何回覆」的失敗才沿用；daemon 明確回了（成功或 ApiError）就作廢，下一次是新的動作（#367）。
    const reqKey = `send:${botId}:${sendNow ? 1 : 0}:${text}\u0000${attachments.join(',')}`
    const crid = createRequestId(reqKey)
    try {
      const res = await api.sendPrompt(botId, text, crid, attachments, sendNow, startIfStopped)
      settleCreateRequest(reqKey)
      // 送出鍵沒生效（not_sent）／不知道生效沒有（unknown）：這一則已是 failed、沒有照一般方式送出（#120）。
      const fell = sendNow ? sendNowFellThrough(res.send_now) : null
      if (fell) {
        get().notify('error', fell.text)
        void get().loadMessages(botId)
        return fell.consumed
      }
      // 沒插成隊時 daemon 照舊送出（閒著的 bot）；為什麼沒插隊要講出來，不然使用者以為打斷了。
      if (sendNow && res.send_now && res.send_now !== 'interrupted' && res.send_now !== 'idle') {
        get().notify('info', '沒有插隊：這顆 bot 的 claude 還沒有 send-now 鍵（2.1.275 起），訊息照一般方式送出。')
      }
      if (res.delivery === 'unknown') {
        get().notify('error', '訊息已送出但送達狀態未知（delivery=unknown），需先放棄該回合才能再送。')
      }
      if (res.delivery === 'failed') {
        // REVIEW B10: turn already failed; seeding a local in_flight turn would lock the composer.
        get().notify('error', '訊息未送達（delivery=failed），請確認 agent 狀態後重試。')
        void get().loadMessages(botId)
        return false
      }
      // daemon 收下了、還沒送（issue #122：它會自己啟動 bot）：先記成排隊中，輸入框馬上換成「啟動中」那一條。
      if (res.delivery === 'queued') {
        set((s) => noteQueuedTurn(s, botId, res.turn_id, crid, startIfStopped))
        return true
      }
      const delivery = res.delivery
      // Patch the turn map so the composer locks even if the socket frame is slow.
      set((s) => noteInFlightTurn(s, botId, res.turn_id, crid, get().runs[botId]?.id ?? null, delivery))
      return true
    } catch (e) {
      if (e instanceof ApiError) settleCreateRequest(reqKey)
      // 送了、結果還沒寫進 DB（503）：不是沒送。回 false 會讓輸入框留著同一段字、排隊的被放回去，之後又送一次。
      const un = asUncommittedSend(e)
      if (un) {
        get().notify('error', uncommittedSendText(un))
        if (un.sent === false) {
          void get().loadMessages(botId)
          return false
        }
        // 回合在 DB 裡還是 in_flight＋pending（欠著的結果之後補）：先鎖上輸入框，別等 socket 那一幀。
        if (un.delivery) {
          const seen: TurnDelivery = un.delivery === 'unknown' ? 'unknown' : 'pending'
          set((s) => noteInFlightTurn(s, botId, un.turnId, crid, get().runs[botId]?.id ?? null, seen))
        }
        return true
      }
      // claude 停在登入選單：通知講白，不要只給「HTTP 409」。
      if (e instanceof ApiError && e.status === 409 && e.body.reason === 'needs_login') {
        get().notify('error', typeof e.body.message === 'string' ? e.body.message : '這個 claude 還沒登入，先到「終端」分頁完成登入。')
        return false
      }
      // 插不了隊（不是 claude、CLI 比 2.1.275 舊、版本還不知道）：daemon 已經說了原因，照抄比「HTTP 409」有用。
      if (e instanceof ApiError && e.status === 409 && typeof e.body.send_now_message === 'string') {
        get().notify('error', e.body.send_now_message)
        return false
      }
      get().notify('error', errText(e))
      return false
    }
  },

  async sendKeys(botId, keys) {
    try {
      await sendWithFreshRun(get, botId, (runId) => api.sendKeys(botId, keys, runId))
    } catch (e) {
      get().notify('error', `送出按鍵失敗：${errText(e)}`)
    }
  },

  async sendText(botId, text, enter) {
    try {
      await sendWithFreshRun(get, botId, (runId) => api.sendText(botId, text, enter, runId))
      return true
    } catch (e) {
      get().notify('error', `送出文字失敗：${errText(e)}`)
      return false
    }
  },

  async abandonTurn(botId, turnId) {
    // 連點：第二下撞 409（已經放棄了）會跳一則原始英文錯誤。
    await guarded(set, get, `abandon:${turnId}`, async () => {
      await api.abandonTurn(turnId)
      await get().loadMessages(botId)
    })
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

  async removeIdentity(name, host = 'local') {
    try {
      await api.deleteIdentity(name, host)
      await get().refreshState()
      get().notify('info', host && host !== 'local' ? `已刪除 ${host} 的身份 ${name}` : `已刪除身份 ${name}`)
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
    const key = `add-project:${input.path}`
    if (get().busy[key]) return false
    set((st) => ({ busy: { ...st.busy, [key]: true } }))
    try {
      await api.createProject(input)
      await get().refreshState()
      get().notify('info', `已新增 Project ${input.label || input.path}`)
      return true
    } catch (e) {
      get().notify('error', errText(e))
      return false
    } finally {
      set((st) => {
        const busy = { ...st.busy }
        delete busy[key]
        return { busy }
      })
    }
  },

  async addBot(projectId, input) {
    const key = `add-bot:${projectId}:${input.name}`
    if (get().busy[key]) return null
    set((st) => ({ busy: { ...st.busy, [key]: true } }))
    // 冪等鍵（#352）：同一個動作（快速新增：專案＋kind＋身分，名字只是提示）失敗後重試沿用同一個鍵，成功才作廢。
    const reqKey = input.name_auto ? `add:${projectId}:${input.kind}:${input.identity ?? ''}` : `add:${projectId}:${input.name}`
    try {
      const { id, name } = await api.createBot(projectId, { ...input, client_request_id: createRequestId(reqKey) })
      settleCreateRequest(reqKey)
      await get().refreshState()
      if (id) set({ selectedBotId: id })
      get().notify('info', `已新增 Bot ${name}`)
      return id || null
    } catch (e) {
      // daemon 說這個鍵已經是另一件事（例如設定變了）：作廢，下一次是新的動作。
      if (String(e).includes('request_id_reused')) settleCreateRequest(reqKey)
      get().notify('error', errText(e))
      return null
    } finally {
      set((st) => {
        const busy = { ...st.busy }
        delete busy[key]
        return { busy }
      })
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
    const name = `${stem}-${n}`
    const key = `clone:${botId}`
    if (s.busy[key]) return null
    // 先放灰色佔位列（假 id）在本尊後面；`refreshState` 保留它到同名真 bot 出現。
    const tempId = `pending:${Date.now().toString(36)}`
    const placeholder: Bot = { ...bot, id: tempId, name, parent_bot_id: null, pending: true }
    const dropPlaceholder = () =>
      set((st) => ({
        bots: st.bots.filter((b) => b.id !== tempId),
        botOrder: { ...st.botOrder, [bot.project_id]: (st.botOrder[bot.project_id] ?? []).filter((id) => id !== tempId) },
      }))
    set((st) => {
      const ids = botsOfProject(st, bot.project_id).map((b) => b.id)
      const at = ids.indexOf(botId)
      const order = at >= 0 ? [...ids.slice(0, at + 1), tempId, ...ids.slice(at + 1)] : [...ids, tempId]
      return { bots: [...st.bots, placeholder], botOrder: { ...st.botOrder, [bot.project_id]: order }, busy: { ...st.busy, [key]: true } }
    })
    try {
      const created = await api.createBot(bot.project_id, {
        name,
        // 名字是瀏覽器從舊清單算的：回應遺失後重送時清單已同步、算出來的名字會變，讓 daemon 往後找（也不算請求內容，#352）。
        name_auto: true,
        client_request_id: createRequestId(key),
        kind: bot.kind,
        model: bot.model,
        effort: bot.effort,
        fast: bot.fast,
        persona: bot.persona,
        instruction_files: bot.instruction_files,
        identity: bot.identity,
        env: bot.env,
        autostart: bot.autostart,
        auto_approve: bot.auto_approve,
      })
      // 舊版 `createBot` 回 id 字串，新版回 `{id, name}`；兩種都吃。
      const c = created as unknown as string | { id: string }
      const id = typeof c === 'string' ? c : c.id
      settleCreateRequest(key)
      if (!id) {
        dropPlaceholder()
        return null
      }
      await get().refreshState()
      if (get().bots.some((b) => b.id === tempId)) dropPlaceholder()
      // daemon 建 bot 一律接在專案最後；分身要排在本尊正下方（使用者 2026-09-15），存成正式順序。
      const rest = botsOfProject(get(), bot.project_id).map((b) => b.id).filter((x) => x !== id)
      get().moveBot(id, rest[rest.indexOf(botId) + 1] ?? null)
      set({ selectedBotId: id })
      get().notify('info', `已新增 Bot ${name}`)
      // 啟動不等：CLI 就緒要好幾秒，交給側欄燈號。
      void get().startBot(id)
      return id
    } catch (e) {
      dropPlaceholder()
      get().notify('error', errText(e))
      return null
    } finally {
      // 其他路徑都是 delete；設成 false 會讓 busy 表每 clone 一次多一個永久 key。
      set((st) => {
        const busy = { ...st.busy }
        delete busy[key]
        return { busy }
      })
    }
  },

  async forkBot(botId) {
    const bot = get().bots.find((b) => b.id === botId)
    if (!bot) return null
    let id: string | null = null
    await guarded(set, get, `fork:${botId}`, async () => {
      const res = await api.forkBot(botId).catch((e: unknown) => {
        throw new Error(forkErrText(e))
      })
      id = res.id || null
      await get().refreshState()
      if (!id) return
      set({ selectedBotId: id })
      if (res.start_error) {
        get().notify('error', `已從 ${bot.name} fork 出 ${res.name}，但沒有啟動：${res.start_error}`)
      } else {
        get().notify('info', `已從 ${bot.name} fork 出 ${res.name}，接續它的對話脈絡`)
      }
    })
    return id
  },

  async promoteBot(botId) {
    const bot = get().bots.find((b) => b.id === botId)
    if (!bot) return null
    let id: string | null = null
    await guarded(set, get, `promote:${botId}`, async () => {
      const res = await api.promoteBot(botId).catch((e: unknown) => {
        throw new Error(promoteErrText(e))
      })
      id = res.id || null
      await get().refreshState()
      if (!id) return
      set({ selectedBotId: id })
      get().notify('info', `已把子 agent ${bot.name} 升級成頂層 bot ${res.name}，接續同一段對話`)
    })
    return id
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

  async restartBot(botId, resumeNative) {
    let ok = false
    await guarded(set, get, `restart:${botId}`, async () => {
      try {
        await api.restartBot(botId, resumeNative)
      } catch (e: unknown) {
        // 接不回原對話（`cannot_resume`）：daemon 不會啟動，退回不帶 `resume=native` 重送一次，
        // 好過使用者按了「立即重啟」卻什麼都沒發生。
        if (resumeNative && e instanceof ApiError && e.body.reason === 'cannot_resume') {
          await api.restartBot(botId)
        } else if (isRunStateUncommitted(e, 'restart') || isRunStateUncommitted(e, 'start') || isRunStateUncommitted(e, 'stop')) {
          get().notify('error', runStateUncommittedText(e))
        } else {
          throw e
        }
      }
      await get().refreshState()
      ok = true
    })
    return ok
  },

  clearRestartBatch: () => set({ restartBatch: null }),

  async restartIdleBots() {
    await guarded(set, get, 'restart-idle', async () => {
      const plan = await api.restartIdleBots()
      // 已經有一批在跑（daemon 同時只准一批）：回的是那一批的 id、total 是 0。不能當成「沒有要重啟的」
      // 把進度蓋掉或跳「沒有閒置的 Bot」——接著看那一批的事件就好。
      if (plan.already_running) {
        set({ restartBatch: joinRunningBatch(get().restartBatch, plan.batch_id) })
        get().notify('info', '已經有一批重啟在跑，這次不另開，進度照那一批顯示')
        return
      }
      // 按鈕數字從此用 daemon 的計畫，不用前端估的。
      set({
        restartBatch: {
          id: plan.batch_id,
          total: plan.total,
          done: 0,
          current: null,
          ok: [],
          failed: [],
          skipped: plan.skipped,
          finished: plan.total === 0,
        },
      })
      if (plan.total === 0) {
        get().notify('info', plan.skipped.length > 0 ? '沒有閒置的 Bot 可以重啟（都在忙）' : '沒有等著套用更新的 Bot')
      }
    })
  },

  async removeBot(botId) {
    // SPEC: selection moves to the next bot in the same project, else null.
    await guarded(set, get, `remove:${botId}`, async () => {
      const s0 = get()
      const bot = s0.bots.find((b) => b.id === botId)
      const siblings = bot ? s0.bots.filter((b) => b.project_id === bot.project_id) : []
      const i = siblings.findIndex((b) => b.id === botId)
      const next = (siblings[i + 1] ?? siblings[i - 1] ?? null)?.id ?? null
      const name = bot?.name ?? 'Bot'
      try {
        await api.deleteBot(botId)
        // 軟刪除（只設 `deleted_at`），所以復原是真的復原。
        get().notify('info', `已刪除 ${name}`, {
          label: '復原',
          run: async () => {
            try {
              await api.restoreBot(botId)
              await get().refreshState()
              get().selectBot(botId)
            } catch (e) {
              get().notify('error', `復原失敗：${errText(e)}`)
            }
          },
        })
        set((s) => {
          const drafts = withoutKey(s.drafts, `bot:${botId}`)
          const draftCursors = withoutKey(s.draftCursors, `bot:${botId}`)
          writeDrafts(s.drafts, drafts)
          writeDraftCursors(s.draftCursors, draftCursors)
          return {
            selectedBotId: s.selectedBotId === botId ? next : s.selectedBotId,
            settingsBotId: s.settingsBotId === botId ? null : s.settingsBotId,
            drafts,
            draftCursors,
          }
        })
        await get().refreshState()
        // `refreshState` falls back to `bots[0]`; honour "no sibling left" instead.
        if (next === null && get().selectedBotId !== null && !get().bots.some((b) => b.id === botId)) {
          set({ selectedBotId: null })
        }
      } catch (e) {
        get().notify('error', errText(e))
      }
    })
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
        writeDrafts(s.drafts, drafts)
        writeDraftCursors(s.draftCursors, draftCursors)
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
      /* keep what it had */
    }
  },

  async loadMem() {
    try {
      set({ mem: await api.fetchMem() })
    } catch {
      /* keep what it had */
    }
  },

  async loadModels(kind, host, identity) {
    // 身份進快取 key：`default_effort` 因身份而異（SPEC §17.1）。
    const key = modelsKey(kind, host, identity)
    const cached = get().models[key]
    // issue #26：失敗不是永久的，null 只擋 MODELS_RETRY_MS。
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
        writeDraftCursors(s.draftCursors, draftCursors)
        return { draftCursors }
      }
      const drafts = text ? { ...s.drafts, [key]: text } : withoutKey(s.drafts, key)
      const draftCursors = text ? s.draftCursors : withoutKey(s.draftCursors, key)
      writeDrafts(s.drafts, drafts)
      if (!text) writeDraftCursors(s.draftCursors, draftCursors)
      return { drafts, draftCursors }
    })
  },

  setDraftCursor: (key, start, end = start) => {
    set((s) => {
      const text = s.drafts[key] ?? ''
      if (!text) {
        if (!s.draftCursors[key]) return {}
        const draftCursors = withoutKey(s.draftCursors, key)
        writeDraftCursors(s.draftCursors, draftCursors)
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
      writeDraftCursors(s.draftCursors, draftCursors)
      return { draftCursors }
    })
  },

  readTerminal: (botId, source, lines) => api.fetchTerminal(botId, source, lines),

  // 主機 shell
  async openHostShell(host, cwd) {
    let ok = false
    await guarded(set, get, `shell:${host}`, async () => {
      // 指定 cwd 就明確要新開，不接回舊的。
      const existing = cwd ? [] : await api.fetchHostShells(host)
      const reuse = existing.length > 0 ? existing[existing.length - 1] : null
      const shell = reuse ?? (await api.openHostShell(host, cwd))
      // 不動 selectedBotId：shell 掛在目前 bot 標題列底下（2026-09-08）。
      set({ shellView: { host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null })
      ok = true
    })
    return ok
  },

  async loginIdentity(host, identity) {
    return identityAuth(set, get, host, identity, 'login')
  },

  async logoutIdentity(host, identity) {
    return identityAuth(set, get, host, identity, 'logout')
  },

  async loadIdentityPrefs() {
    try {
      set({ disabledIdentities: await api.fetchDisabledIdentities() })
    } catch {
      // 讀不到就當作沒有人被停用：選單多幾個選項，比整個面板掛掉好。
    }
  },

  async setIdentityDisabled(host, kind, name, disabled) {
    const key = api.identityPrefKey(host, kind, name)
    await guarded(set, get, `identity-disabled:${key}`, async () => {
      await api.setIdentityDisabled(host, kind, name, disabled)
      // 事件也會送一份；先自己更新，按下去才不會等一個來回。
      set((s) => ({
        disabledIdentities: disabled
          ? s.disabledIdentities.includes(key)
            ? s.disabledIdentities
            : [...s.disabledIdentities, key]
          : s.disabledIdentities.filter((k) => k !== key),
      }))
    })
  },

  sidePanes: {},
  unownedPanes: [],

  // 一次讀全部再依 `project_id` 分：側欄每個專案各打一支、專案頁再自己打一支，三份各自過期（review M3）。
  refreshPanes: singleFlight(async () => {
    let all: ProjectPane[]
    try {
      all = await api.fetchAllPanes()
    } catch {
      // 舊 daemon 沒有這支端點：選單不多出東西，其他照舊。
      return
    }
    // scratch 只有 `?unowned=1` 會標；讀不到就退回「對不到專案的全部列、不標」。
    const unowned = await api.fetchUnownedPanes().catch(() => all.filter((p) => !p.project_id))
    set({ sidePanes: groupByProject(all), unownedPanes: unowned })
  }),

  async closeTracedPane(pane, confirm) {
    try {
      await api.closePane(pane.pane_id, pane.host, confirm)
    } catch (e) {
      // 清單讀到時還是 shell、之後才開了 dev server：daemon 擋下來並附上最新那列，拿它來問人（review L2）。
      const fresh = api.servicePaneConflict(e)
      if (fresh) {
        set((s) => withPane(s, fresh))
        return fresh
      }
      if (e instanceof ApiError && e.status === 404) {
        get().paneGone(pane.host, pane.pane_id)
        return 'closed'
      }
      get().notify('error', `關閉失敗：${errText(e)}`)
      return null
    }
    dropPane(set, pane.host, pane.pane_id)
    void get().refreshPanes()
    return 'closed'
  },

  paneGone(host, paneId) {
    dropPane(set, host, paneId)
    get().notify('info', '這顆 pane 已經關掉了')
    void get().refreshPanes()
  },

  // 同一個 shell 面板：面板只認 (host, paneId)，白名單在 daemon 那一側（`shell::registered`）。
  viewPane: (pane) =>
    set({
      shellView: { host: pane.host, paneId: pane.pane_id, cwd: pane.cwd ?? '', readOnly: paneReadOnly(pane), traced: true },
      settingsBotId: null,
    }),

  viewHostShell: (shell) =>
    set({ shellView: { host: shell.host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null }),

  closeShellView: () => set({ shellView: null }),

  lockShellView(host, paneId, reason) {
    set((s) =>
      s.shellView && s.shellView.host === host && s.shellView.paneId === paneId
        ? { shellView: { ...s.shellView, readOnly: true, readOnlyReason: reason } }
        : {},
    )
  },

  async restoreShellView() {
    const v = get().shellView
    if (!v) return
    // 回來時 pane 可能已經換了樣子：唯讀／是不是自己開的都照 daemon 現在說的，不沿用 localStorage 的舊值。
    const still = (next: ShellView | null) =>
      set((s) => (s.shellView && s.shellView.host === v.host && s.shellView.paneId === v.paneId ? { shellView: next } : {}))
    try {
      const alive = await api.fetchHostShells(v.host)
      if (alive.some((sh) => sh.pane_id === v.paneId)) return still({ ...v, readOnly: false, traced: false })
      // daemon 自己開的那份清單只在記憶體，重啟就空了；被 trace 的 pane 活得比它久（§6.5e）。
      const tracked = (await api.fetchAllPanes()).find((p) => p.host === v.host && p.pane_id === v.paneId)
      still(tracked ? { ...v, readOnly: paneReadOnly(tracked), traced: true } : null)
    } catch {
      // 暫時連不上就留著：面板會顯示讀取失敗。
    }
  },

  async endHostShell(host, paneId, confirm = false) {
    let needs: api.CloseNeedsConfirm | null = null
    await guarded(set, get, `shell:${host}:${paneId}`, async () => {
      // 面板自己開的那份清單只在 daemon 記憶體：重啟後這顆還活著（pane 表認得，面板照常顯示），
      // `DELETE` 卻在清單裡找不到而回 200、什麼都沒關，面板收掉看起來像結束了（review M2）。
      // 不在那份清單裡就改走 pane 表的關閉。
      //
      // **不預設帶 confirm**（AGM 驗收 9f05b03）：在 listen 的服務 pane、或讀不到它在跑什麼時，daemon 要先回 409
      // 讓人看過 port 再確認——第一個「結束 shell」確認框只講了「指令會結束」，沒講「裡面是 dev server」。
      try {
        const own = await api.fetchHostShells(host)
        if (own.some((sh) => sh.pane_id === paneId)) {
          await api.closeHostShell(host, paneId, confirm)
        } else {
          try {
            await api.closePane(paneId, host, confirm)
          } catch (e) {
            // 兩份都不認得＝已經不在了；其他錯誤照常往上丟。
            if (!(e instanceof ApiError && e.status === 404)) throw e
          }
        }
      } catch (e) {
        needs = api.closeNeedsConfirm(e)
        // 要人再確認不是失敗：面板留著、不跳錯誤，回傳給面板去問。
        if (needs) return
        throw e
      }
      set((s) =>
        s.shellView && s.shellView.host === host && s.shellView.paneId === paneId ? { shellView: null } : {},
      )
    })
    return needs
  },

  // 群組任務（mission）
  async loadMissions(projectId) {
    let run = missionListLoads.get(projectId)
    if (!run) {
      // 一次湧進好幾則 `mission_updated` 時只打一輪（外加結尾補一輪），不是每則都三支請求。
      run = singleFlight(async () => {
        if (!get().missionsSupported) return
        try {
          const [open, done, cancelled] = await Promise.all([
            api.fetchMissions(projectId, 'open', MISSION_OPEN_LIMIT),
            api.fetchMissions(projectId, 'done', MISSION_CLOSED_LIMIT),
            api.fetchMissions(projectId, 'cancelled', MISSION_CLOSED_LIMIT),
          ])
          set((s) => ({
            missions: { ...s.missions, [projectId]: newestFirst([...open, ...done, ...cancelled]) },
            missionsCapped: {
              ...s.missionsCapped,
              [projectId]: { done: done.length >= MISSION_CLOSED_LIMIT, cancelled: cancelled.length >= MISSION_CLOSED_LIMIT },
            },
          }))
        } catch (e) {
          if (api.isMissionsUnsupported(e)) {
            set({ missionsSupported: false })
            return
          }
          get().notify('error', `載入任務失敗：${errText(e)}`)
        }
      })
      missionListLoads.set(projectId, run)
    }
    await run()
  },

  async refreshLoadedMissions() {
    const s = get()
    if (!s.missionsSupported) return
    // 斷線期間交辦可能已經換了好幾個階段：卡片會一直停在舊的那一格，直到使用者重整（review3 c1 M5）。
    await Promise.all([
      ...Object.keys(s.missions).map((pid) => s.loadMissions(pid)),
      ...Object.keys(s.missionDetail).map((id) => s.loadMission(id)),
    ])
  },

  async loadMission(missionId) {
    let run = missionLoads.get(missionId)
    if (!run) {
      // Coalesce concurrent reads but run again after an update received mid-request.
      run = singleFlight(async () => {
        if (!get().missionsSupported) return
        set((s) => ({
          missionLoading: { ...s.missionLoading, [missionId]: true },
          missionLoadErrors: { ...s.missionLoadErrors, [missionId]: '' },
        }))
        try {
          const detail = await api.fetchMission(missionId)
          if (!detail) throw new Error('任務資料不完整')
          set((s) => ({
            missionDetail: { ...s.missionDetail, [missionId]: detail },
            missions: mergeMission(s.missions, detail),
          }))
        } catch (e) {
          if (api.isMissionsUnsupported(e)) {
            set({ missionsSupported: false })
            return
          }
          // 這一筆被清掉了（AGM 結案時常有）：收掉這張卡就好，其他任務跟「交給 AGM」開關照常。
          if (api.isMissionGone(e)) {
            set((s) => ({
              missionDetail: withoutKey(s.missionDetail, missionId),
              missions: dropMission(s.missions, missionId),
              missionLoadErrors: { ...s.missionLoadErrors, [missionId]: '這筆任務已經不在了' },
            }))
            return
          }
          set((s) => ({ missionLoadErrors: { ...s.missionLoadErrors, [missionId]: errText(e) } }))
        } finally {
          set((s) => ({ missionLoading: { ...s.missionLoading, [missionId]: false } }))
        }
      })
      missionLoads.set(missionId, run)
    }
    await run()
  },

  async startMission(projectId, input) {
    try {
      // crid 交給 `missionRequests`：每按一次就換一個（例如 `Date.now()`）等於把 API 的冪等關掉——
      // daemon 已經 commit 但回應在路上斷掉時，使用者照提示再按一次就會多出第二筆任務。
      // 選項也算進 key：同一段文字改了執行者或交付方式再送是另一個要求。沿用舊 crid 的話 daemon 不比對內容、
      // 直接回舊的那筆（`created:false` 不提示），輸入框還被清掉（第二輪 review 修正驗證 #6）。
      const variant = [input.delivery_mode, input.executor_kind, input.on_5h_limit, input.max_rounds ?? ''].join('|')
      const { mission, created } = await sendMissionRequest(`create:${variant}`, projectId, input.text, (id) =>
        api.createMission(projectId, { ...input, client_request_id: id }),
      )
      if (!mission) return null
      set((s) => ({ missions: mergeMission(s.missions, mission) }))
      // 同 client_request_id 重送回 `created:false`，不再提示。
      if (created) get().notify('info', '已交給 AGM，任務卡會顯示進度')
      return mission.id
    } catch (e) {
      if (api.isMissionsUnsupported(e)) {
        set({ missionsSupported: false })
        get().notify('error', '這個 daemon 還沒有群組任務（需要更新）')
        return null
      }
      const reason = api.missionRejectReason(e)
      get().notify(
        'error',
        reason === 'remote_not_supported' ? '群組任務目前只支援本機專案' : `交給 AGM 失敗：${errText(e)}`,
      )
      return null
    }
  },

  async controlMission(missionId, action) {
    // 連點：第二下會撞「已經暫停／已經取消」的 409，跳一則多餘的失敗通知。
    await guarded(
      set,
      get,
      `mission:${missionId}`,
      async () => {
        // daemon 的 pause 必須帶 `reason`：以前送 `{}`，反序列化就 422，按鈕從上線起沒成功過（review3 c1 M4）。
        const mission = await api.controlMission(missionId, action, action === 'pause' ? { reason: MISSION_USER_PAUSE } : undefined)
        if (mission) set((s) => ({ missions: mergeMission(s.missions, mission) }))
        await get().loadMission(missionId)
      },
      (e) => `任務操作失敗：${errText(e)}`,
    )
  },

  async answerMission(missionId, text) {
    try {
      // 一支 API 原子完成記錄＋放行＋喚醒（兩支會斷成半套）；crid 讓重送安全。
      await sendMissionRequest('answer', missionId, text, (id) => api.answerMission(missionId, { text, client_request_id: id }))
      await get().loadMission(missionId)
      return true
    } catch (e) {
      get().notify('error', `回覆任務失敗：${errText(e)}`)
      return false
    }
  },

  async askMission(missionId, text) {
    try {
      await sendMissionRequest('ask', missionId, text, (id) => api.askMission(missionId, { text, client_request_id: id }))
      await get().loadMission(missionId)
      return true
    } catch (e) {
      get().notify('error', `追問失敗：${errText(e)}`)
      return false
    }
  },

  async reviseMission(missionId, text) {
    try {
      const next = await sendMissionRequest('revise', missionId, text, (id) => api.reviseMission(missionId, { text, client_request_id: id }))
      // 兩邊都要重讀：原成果多了「已開續作」的連結，新任務要進清單。
      await get().loadMission(missionId)
      if (next) {
        await get().loadMission(next.id)
        await get().loadMissions(next.project_id)
        get().notify('info', '已開一筆續作任務，原本的成果保持不變')
      }
      return next?.id ?? null
    } catch (e) {
      get().notify('error', `追加修改失敗：${errText(e)}`)
      return null
    }
  },
}))

/** 任務不在了：從已載入的清單裡拿掉，沒載過的專案不動。 */
function dropMission(map: Record<string, Mission[]>, missionId: string): Record<string, Mission[]> {
  const out: Record<string, Mission[]> = {}
  let changed = false
  for (const [pid, list] of Object.entries(map)) {
    const next = list.filter((m) => m.id !== missionId)
    if (next.length !== list.length) changed = true
    out[pid] = next
  }
  return changed ? out : map
}

/**
 * 清單沒載過就不併：憑一筆建出半份清單會讓任務看起來只有一筆。
 *
 * 已經在清單裡的**原地替換**：以前一律搬到最前面，點開已完成清單的第 6 筆，回應一到它就跳到第 1 位、
 * 畫面在手指底下移動（review3 c1 L10）。找不到的（剛建、或從連結點進來的舊任務）才插到最前。
 */
function mergeMission(map: Record<string, Mission[]>, mission: Mission): Record<string, Mission[]> {
  const list = map[mission.project_id]
  if (!list) return map
  const at = list.findIndex((m) => m.id === mission.id)
  const next = at < 0 ? [mission, ...list] : list.map((m, i) => (i === at ? mission : m))
  return { ...map, [mission.project_id]: next }
}

/** open／done／cancelled 三份合成一份，新的在前；同一筆只留一次。 */
function newestFirst(list: Mission[]): Mission[] {
  const seen = new Set<string>()
  return list
    .filter((m) => (seen.has(m.id) ? false : (seen.add(m.id), true)))
    .sort((a, b) => (a.created_at < b.created_at ? 1 : a.created_at > b.created_at ? -1 : 0))
}

// One subscription instead of a write at every mutation site (covers future ones too).
let lastSelection = initialSelection
let lastShellView = initialShellView
useStore.subscribe((s) => {
  if (s.shellView !== lastShellView) {
    lastShellView = s.shellView
    writeShellView(s.shellView)
  }
  if (
    s.selectedBotId === lastSelection.botId &&
    s.selectedProjectId === lastSelection.projectId
  ) {
    return
  }
  lastSelection = { botId: s.selectedBotId, projectId: s.selectedProjectId }
  writeSelection(lastSelection)
})

type SetFn = (partial: Partial<StoreState> | ((s: StoreState) => Partial<StoreState>)) => void
type GetFn = () => StoreState

function reportStateRefreshError(set: SetFn, get: GetFn, e: unknown) {
  const text = `同步狀態失敗：${errText(e)}`
  set({ stateStale: true })
  if (lastRefreshError === text) return
  lastRefreshError = text
  get().notify('error', text)
}

async function guarded(set: SetFn, get: GetFn, key: string, fn: () => Promise<void>, toText: (e: unknown) => string = errText) {
  if (get().busy[key]) return
  set((s) => ({ busy: { ...s.busy, [key]: true } }))
  try {
    await fn()
  } catch (e) {
    get().notify('error', toText(e))
  } finally {
    set((s) => {
      const busy = { ...s.busy }
      delete busy[key]
      return { busy }
    })
  }
}

/** 登入與登出走同一段：都是開一個臨時 pane 看 CLI 跑完，差別只在打哪一行指令。 */
async function identityAuth(set: SetFn, get: GetFn, host: string, identity: string, op: 'login' | 'logout') {
  let ok = false
  await guarded(set, get, `identity-${op}:${host}:${identity}`, async () => {
    const shell = op === 'login' ? await api.loginIdentity(host, identity) : await api.logoutIdentity(host, identity)
    set({ shellView: { host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null })
    ok = true
  })
  return ok
}

let disconnect: (() => void) | null = null
let openedOnce = false
let quotaSweep: ReturnType<typeof setInterval> | null = null
const QUOTA_SWEEP_MS = 5 * 60_000

function connectSocket(set: SetFn, get: GetFn) {
  disconnect?.()
  disconnect = api.openSocket({
    since: () => get().lastSeq,
    onStatus: (socket) => {
      if (socket === 'open') {
        resetStateSeq()
        // Re-fetch on every open: a failed frame already advanced lastSeq; the snapshot repairs the gap.
        set({ socket, stateStale: false })
        void get().refreshState()
        void get().refreshLoadedMissions()
        if (openedOnce) void reloadLoadedConversations(get)
        openedOnce = true
        flushUnsentReads()
        // 額度也整份重抓（取代，不合併）：WS 只會推「某個 key 更新了」，daemon 刪掉的 key 永遠不會
        // 通知。2026-09-14 daemon 重啟清掉 `codex:cc1` 之後，開著的分頁標題列仍一直顯示它。
        void get().loadQuota()
        return
      }
      set({ socket })
    },
    onFrame: (frame) => handleFrame(set, get, frame),
  })
}

function loadedBotIdsOf(get: GetFn): string[] {
  return Object.keys(get().loadedBots).filter((botId) => get().loadedBots[botId])
}

/**
 * 斷線重連後補訊息（#368）：daemon 重啟沒有世代標記，重連時新 daemon 的 seq 若已超過我們記的 `lastSeq`，
 * 它會當成「只差幾則」照補，舊 daemon 尾巴那段訊息永遠不會來，也不會 `resync`。`refreshState` 只補狀態不補訊息，
 * 所以重連（不是第一次連上）時已載入的對話一律重抓。
 */
export async function reloadLoadedConversations(get: GetFn): Promise<void> {
  for (const botId of loadedBotIdsOf(get)) await get().loadMessages(botId)
  const proj = get().selectedProjectId
  if (proj) await get().loadGroupMessages(proj)
}

const resyncTrigger = (() => {
  let ctx: { set: SetFn; get: GetFn } | null = null
  const runner = createResyncRunner(async () => {
    const { set, get } = ctx!
    resetStateSeq()
    try {
      const loadedBotIds = loadedBotIdsOf(get)
      await get().refreshState()
      await get().loadQuota()
      for (const botId of loadedBotIds) await get().loadMessages(botId)
      const proj = get().selectedProjectId
      if (proj) await get().loadGroupMessages(proj)
      await get().refreshLoadedMissions()
    } catch (e) {
      reportStateRefreshError(set, get, e)
    }
  })
  return (set: SetFn, get: GetFn) => {
    ctx = { set, get }
    runner()
  }
})()

function viewingBot(s: StoreState, botId: string): boolean {
  return s.selectedBotId === botId && !s.selectedProjectId && !s.shellView
}

/** 回合完成（訊息與 `turn_updated` 都會觸發，`takeTurnCompletion` 只算一次）；沒在看就記未讀。 */
function noteTurnDone(set: SetFn, get: GetFn, botId: string, turnId: string) {
  if (viewingBot(get(), botId) && windowActive()) {
    get().markBotRead(botId)
    return
  }
  if (!takeTurnCompletion(botId, turnId)) return
  set((s) => ({ botUnread: { ...s.botUnread, [botId]: (s.botUnread[botId] ?? 0) + 1 } }))
  persistUnread(get())
}

/** 同上，記在專案群組（§13.6）。 */
function noteGroupTurnDone(set: SetFn, get: GetFn, projectId: string, turnId: string) {
  if (get().selectedProjectId === projectId && windowActive()) {
    get().markGroupRead(projectId)
    return
  }
  if (!takeTurnCompletion(`group:${projectId}`, turnId)) return
  set((s) => ({ groupUnread: { ...s.groupUnread, [projectId]: (s.groupUnread[projectId] ?? 0) + 1 } }))
  persistUnread(get())
}

/** 只有群組回合才記到專案（`store/groupUnread.ts`）；要抓訊息確認時非同步記。 */
function noteGroupCompletion(set: SetFn, get: GetFn, botId: string, turnId: string | null, key: string, turn: Turn | null | undefined) {
  const pid = get().bots.find((b) => b.id === botId)?.project_id
  if (!pid) return
  void confirmGroupTurn(botId, turnId, turn, (id, tid, before) => api.fetchMessages(id, 50, before, { turnId: tid, role: 'user' })).then((yes) => {
    if (yes) noteGroupTurnDone(set, get, pid, key)
  })
}

function handleFrame(set: SetFn, get: GetFn, frame: { seq?: number; type: string; data?: unknown }) {
  if (typeof frame.seq === 'number') {
    set((s) => ({ lastSeq: Math.max(s.lastSeq, frame.seq as number) }))
  }
  const data = frame.data
  switch (frame.type) {
    case 'resync': {
      resyncTrigger(set, get)
      return
    }
    case 'daemon_status': {
      // SPEC §11.6: `{herdr_connected, default_connected, hosts: {<name>: {connected, error?}}}`.
      if (!isRec(data)) return
      set((s) => {
        const patch: Partial<StoreState> = {
          connected: bool(pick(data, 'herdr_connected'), s.connected),
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
    case 'identity_prefs_changed': {
      if (!isRec(data)) return
      const key = api.identityPrefKey(str(pick(data, 'host')), str(pick(data, 'kind')), str(pick(data, 'identity')))
      const on = bool(pick(data, 'disabled'), false)
      set((s) => ({
        disabledIdentities: on
          ? s.disabledIdentities.includes(key)
            ? s.disabledIdentities
            : [...s.disabledIdentities, key]
          : s.disabledIdentities.filter((k) => k !== key),
      }))
      return
    }
    case 'host_changed': {
      if (!isRec(data)) return
      const name = str(pick(data, 'name'))
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
          ...(pick(data, 'herdr') !== undefined ? { localHerdr: toHerdrVersion(pick(data, 'herdr')) } : {}),
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
        // Added elsewhere or deletion notice: the full record comes from `GET /api/state`.
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
      const wasWorking = get().runs[botId]?.agent_status === 'working'
      const defaultSession = str(record ? pick(record, 'herdr_session') : undefined) === 'default' || bot?.herdr_session === 'default'
      // #20: `connected` is the bot's host/session; a remote bot must never flip the global herdr flag.
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
      // 自動啟動排隊的訊息：bot 起來、閒著就送（flushQueued 會再檢查一次能不能送）。
      if (run?.state === 'running' && run.agent_status === 'idle' && get().queuedSends[botId]) flushQueued(botId)
      // working → idle 也算回合完成：終端直接對話或沒裝 hook 時不會有 message/turn frame。
      if (!wasWorking && run?.agent_status === 'working') clearHookCompletion(botId)
      if (wasWorking && run?.agent_status === 'idle') {
        // key 規則見 `idleEdgeCompletionKey`（ULID 字典序＝時間序）。
        const map = get().turns[botId] ?? {}
        const turnIds = Object.keys(map)
        const latest = turnIds.length > 0 ? turnIds.reduce((a2, b2) => (a2 > b2 ? a2 : b2)) : null
        const key = idleEdgeCompletionKey(botId, run.id, latest ? map[latest] : null)
        if (key) {
          noteTurnDone(set, get, botId, key)
          noteGroupCompletion(set, get, botId, latest, key, latest ? map[latest] : null)
        }
      }
      return
    }
    case 'message_added': {
      const botId = frameBotId(data)
      const msg = toMessage(unwrap(data, 'message'), botId ?? undefined)
      if (msg) noteGroupPrompt(msg)
      if (!msg || !botId) {
        // Cannot attribute it — reload the visible conversation.
        const sel = get().selectedBotId
        if (sel) void get().loadMessages(sel)
        return
      }
      set((s) => {
        const patch: Partial<StoreState> = {}
        const more: Record<string, boolean> = {}
        // issue #25：滿了從頭截掉，並打開「還有更早的」。
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
        if (Object.keys(more).length > 0) patch.moreMessages = { ...s.moreMessages, ...more }
        return patch
      })
      // 記未讀要在 set 之後：`markBotRead` 的標記要從含這則訊息的清單推出。
      if (completesTurn(msg)) {
        // 沒 `turn_id` 也要跟 `turn_updated` 同 key，否則徽章跳兩下。
        const turnId = completionKey(msg, Object.keys(get().turns[botId] ?? {}))
        markHookCompletion(botId)
        noteTurnDone(set, get, botId, turnId)
        noteGroupCompletion(set, get, botId, msg.turn_id, turnId, get().turns[botId]?.[turnId])
      }
      return
    }
    case 'turn_updated': {
      const botId = frameBotId(data)
      const turn = toTurn(unwrap(data, 'turn'), botId ?? undefined)
      if (!turn || !botId) return
      set((s) => ({
        // issue #25：舊回合沒人讀，不剪會只增不減。
        turns: { ...s.turns, [botId]: pruneTurns({ ...(s.turns[botId] ?? {}), [turn.id]: turn }) },
        liveReply:
          turn.status !== 'in_flight' && s.liveReply[botId]?.turnId === turn.id ? withoutKey(s.liveReply, botId) : s.liveReply,
      }))
      // The turn that was blocking the composer is over: send whatever was queued behind it.
      // `queued` 還沒開始（issue #122：啟動失敗的原因更新也推這一幀）——算成完成會吃掉之後真正的那一次（`takeTurnCompletion` 依 id 去重）。
      if (turn.status !== 'in_flight' && turn.status !== 'queued') {
        flushQueued(botId)
        // 沒有 assistant 訊息的回合（中止、只有終端輸出）也要算完成。
        markHookCompletion(botId)
        noteTurnDone(set, get, botId, turn.id)
        noteGroupCompletion(set, get, botId, turn.id, turn.id, turn)
      }
      return
    }
    case 'turn_progress': {
      // API.md v3.9/v4.1/v4.2
      const botId = frameBotId(data)
      if (!botId || !isRec(data)) return
      const turnId = str(pick(data, 'turn_id'))
      if (!turnId) return
      const text = str(pick(data, 'text'))
      const activity = str(pick(data, 'activity'))
      const alert = str(pick(data, 'alert'))
      const revision = Number(pick(data, 'revision') ?? 0) || 0
      // 每個 frame 都 set 就每個都 render：同 bot 250 ms 內頭一個立刻套、其餘合併到下一拍。
      const pendingKey = botId
      const apply = (trailing: boolean) =>
        set((s) => {
          const prev = s.liveReply[botId]
          // Frames can only move forward within a turn; a new turn always replaces.
          if (prev && prev.turnId === turnId && prev.revision > revision) return {}
          // 合併到下一拍的那一幀：回合若已在這 250ms 內結束（`message_added`／`turn_updated` 清掉了
          // liveReply），別把舊的 partial 寫回去留到下一回合。認不得的 turn（還沒進 map）照套。
          if (trailing) {
            const st = s.turns[botId]?.[turnId]?.status
            if (st !== undefined && st !== 'in_flight') return {}
          }
          return { liveReply: { ...s.liveReply, [botId]: { turnId, text, activity, alert, revision } } }
        })
      const slot = liveThrottle.get(pendingKey)
      if (slot) {
        slot.apply = () => apply(true)
        return
      }
      apply(false)
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
    case 'mission_updated': {
      // 只帶 id 與狀態（API.md），細節重抓一次；清單還沒載過就不主動去載（使用者沒在看）。
      const d = isRec(frame.data) ? frame.data : {}
      const missionId = str(d.mission_id)
      const projectId = str(d.project_id)
      if (!missionId) return null
      queueMicrotask(() => {
        const s = useStore.getState()
        if (s.missionDetail[missionId]) void s.loadMission(missionId)
        // A child finishing changes its parent's continuation button and relation list too.
        for (const detail of Object.values(s.missionDetail)) {
          if (detail.revisions.some((r) => r.id === missionId)) void s.loadMission(detail.id)
        }
        if (projectId && s.missions[projectId]) void s.loadMissions(projectId)
      })
      return null
    }

    // SPEC §6.9：批次重啟的進度。一顆一顆來，`index` 是第幾顆（1-based）。
    case 'bots_restart_progress': {
      if (!isRec(data)) return
      set((s) => {
        const next = s.restartBatch ? restartProgress(s.restartBatch, data) : null
        return next && next !== s.restartBatch ? { restartBatch: next } : {}
      })
      return
    }
    case 'bots_restart_done': {
      if (!isRec(data)) return
      const batchId = str(pick(data, 'batch_id'))
      const skipped = api.toRestartSkips(pick(data, 'skipped'))
      const b = get().restartBatch
      if (!b || b.id !== batchId) return
      const okNames = arr(pick(data, 'ok')).flatMap((v) => (isRec(v) ? [str(pick(v, 'name'))] : []))
      const failed = arr(pick(data, 'failed')).flatMap((v) =>
        isRec(v) ? [{ name: str(pick(v, 'name')), error: str(pick(v, 'error'), '失敗') }] : [],
      )
      // daemon 的最終清單是權威；中途漏掉的 frame 到這裡會被補齊。
      set({
        restartBatch: {
          ...b,
          done: okNames.length + failed.length,
          current: null,
          ok: okNames,
          failed,
          skipped,
          finished: true,
        },
      })
      if (okNames.length + failed.length > 0) {
        const parts = [`成功 ${okNames.length} 顆`]
        if (skipped.length > 0) parts.push(`跳過 ${skipped.length} 顆`)
        if (failed.length > 0) parts.push(`失敗 ${failed.length} 顆`)
        get().notify(failed.length > 0 ? 'error' : 'info', `claude 更新重啟完成：${parts.join(' · ')}`)
      }
      // 重啟過的 bot 換了 run，狀態一次撈回來。
      void get().refreshState()
      return
    }
    case 'preview_changed': {
      if (!isRec(data)) return
      const botId = str(pick(data, 'bot_id'))
      if (!botId) return
      const prev = get().previews[botId] ?? PREVIEW_OFF
      const next = toPreviewEvent(data, prev)
      set((s) => ({ previews: { ...s.previews, [botId]: next } }))
      return
    }
    case 'identities_changed':
    case 'project_changed':
    case 'bot_read':
    case 'bot_changed': {
      void get().refreshState()
      return
    }
    default:
      return
  }
}

/** 一顆 pane 不在了：兩份清單一起拿掉；正開著它的面板也收掉。 */
function dropPane(set: SetFn, host: string, paneId: string) {
  set((s) => ({
    ...withoutPane(s, host, paneId),
    ...(s.shellView?.host === host && s.shellView.paneId === paneId ? { shellView: null } : {}),
  }))
}

/** 退回輸入框的字也要寫進 localStorage：只 `set` 的話，重整一次就沒了。 */
function applyRestore(set: SetFn, get: GetFn, r: RestoreResult) {
  const prev = get().drafts
  set(r.patch)
  if (r.patch.drafts) writeDrafts(prev, r.patch.drafts)
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
    // 送不出去（409 picker_open／dialog_open／needs_login、502、網路錯）就放回去，
    // 別讓文字連附件一起消失；toast 由 sendPrompt 自己講。
    void s.sendPrompt(botId, pending.text, pending.attachments).then((ok) => {
      if (!ok) useStore.getState().restoreQueuedSend(botId, pending)
    })
  }, 350)
}

/** Patch connection state onto the known hosts without losing their config fields. */
function sameHerdr(a: HerdrVersion, b: HerdrVersion): boolean {
  return a.server_version === b.server_version && a.protocol === b.protocol && a.cli_version === b.cli_version && a.mismatch === b.mismatch
}

function mergeHosts(current: Host[], updates: unknown[]): Host[] {
  let changed = false
  const next = current.map((h) => {
    const u = updates.find((x) => isRec(x) && str(pick(x, 'name')) === h.name)
    if (!isRec(u)) return h
    const connected = bool(pick(u, 'connected'), h.connected)
    const error = u.error !== undefined
      ? optStr(pick(u, 'error'))
      : connected
        ? null
        : h.error
    const tools = u.tools !== undefined ? toToolMap(u.tools) : h.tools
    // `host_changed` only carries `identities` when the daemon has a detection result; an
    // absent field means "unchanged", never "no identities".
    const identityStatus = u.identities !== undefined ? toIdentityStatusMap(u.identities) : h.identity_status
    const herdr = u.herdr !== undefined ? toHerdrVersion(u.herdr) : h.herdr
    if (connected === h.connected && error === h.error && tools === h.tools && identityStatus === h.identity_status && sameHerdr(herdr, h.herdr)) {
      return h
    }
    changed = true
    return { ...h, connected, error, tools, identity_status: identityStatus, herdr }
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
  /** 落點要跟頂端 QuotaStrip 同一份規則（`quotaLookup`），否則同一顆 bot 兩邊燈號不一樣。 */
  identities: readonly Identity[] = [],
): QuotaWarning | null {
  const q = quotaForIdentity(quota, host, kind, identity, identities)
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
  /** bot 的模型：Fable 週桶只跟跑 fable 的 bot 有關，opus／sonnet 的列不該掛 `F 0%`（2026-09-09 使用者）。 */
  model: string | null = null,
  /** 同 `botQuotaWarning`：落點只有 `quotaLookup` 一份規則。 */
  identities: readonly Identity[] = [],
): QuotaLevel | null {
  const q = quotaForIdentity(quota, host, kind, identity, identities)
  if (!q) return null
  const onFable = (model ?? '').toLowerCase().includes('fable')
  const pick = (w: { used_pct: number; low: boolean; critical: boolean } | null | undefined, name: string) =>
    w && (w.low || w.critical)
      ? { level: (w.critical ? 'crit' : 'warn') as 'warn' | 'crit', pct: Math.max(0, Math.round(100 - w.used_pct)), window: name }
      : null
  const hits = [pick(q.five_hour, '5h'), pick(q.seven_day, kind === 'grok' ? '週' : '7d'), onFable ? pick(q.fable, 'F') : null].filter(
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
  hiddenBotIds?: readonly string[]
}): string[] {
  // 側欄收起來的不走：⌥↑／⌥↓ 走進一顆畫面上找不到的 bot，使用者只能靠搜尋才回得來。
  const hidden = new Set(state.hiddenBotIds ?? [])
  const out: string[] = []
  for (const p of orderedProjects(state)) for (const b of botsOfProject(state, p.id)) if (!hidden.has(b.id)) out.push(b.id)
  // A bot whose project vanished from the list would otherwise be unreachable by keyboard.
  for (const b of state.bots) if (!hidden.has(b.id) && !out.includes(b.id)) out.push(b.id)
  return out
}

/** The neighbour `dir` steps away, wrapping at both ends; null when there is nothing to move to. */
export function adjacentBotId(
  state: {
    projects: Project[]
    projectOrder: string[]
    bots: Bot[]
    botOrder: Record<string, string[]>
    hiddenBotIds?: readonly string[]
  },
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

/**
 * API.md §5：只有**還在飛**的 `delivery=unknown` 才擋下一則。已經 completed／failed 的 unknown
 * 是 RPC 逾時但 Stop hook 先把回合推成終態的殘影，不該把 composer 鎖成「送達狀態未知」——
 * 那時「放棄該回合」打 abandon 只會拿 409，只能重整。
 */
export function unknownDeliveryTurn(state: StoreState, botId: string): Turn | null {
  const map = state.turns[botId] ?? {}
  for (const t of Object.values(map)) {
    if (t.delivery === 'unknown' && t.status === 'in_flight') return t
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
  // issue #122：已經有一則交給 daemon、在等 bot 起來——再送就排在它後面，不再觸發一次啟動。
  const starting = startingSend(state.turns[botId], state.messages[botId])
  if (!run && starting) {
    return { ...base, disabled: false, queued: true, reason: startingSendLabel(starting).replace(/：$/, '') }
  }
  if (!run) {
    return { ...base, disabled: false, queued: true, autoStart: true, reason: 'Bot 沒在跑：送出會先啟動它，起來後自動送出' }
  }
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
  const queued = Object.values(state.turns[botId] ?? {}).find((t) => t.status === 'queued')
  if (queued) return { ...base, disabled: false, queued: true, reason: '已有訊息排隊中，等它送出後再試' }
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
/** 遠端還沒偵測過 alias 時，沒寫 host 的 config 不能搶這些名字（daemon `tools::SHELL_IDENTITY_NAMES`）。 */
const SHELL_IDENTITY_NAMES = ['cc0', 'cc1', 'cc2', 'cc3', 'cc4', 'cc5', 'cc6']

/**
 * 這台主機能用的身分，**跟 daemon `tools::merge_identities` 同一條優先序**（SPEC §16.2），同名前面的贏：
 * ① config 明寫這一台的 → ② 本機才有：沒寫 host 的 config → ③ 那台自己的 shell `ccN`
 * → ④ 遠端才有：沒寫 host 的 config（讓位給那台同名的；名字是 cc0…cc6 時要等那台偵測過才給）。
 *
 * 以前前端是「沒寫 host＝只有本機」，daemon 改成「所有主機都適用」之後（dca3c4c），遠端的 bot 在
 * daemon 那邊拿得到身分，選單與額度條卻看不到它。
 */
export function identitiesOfHost(all: Identity[], status: IdentityStatusMap, host = LOCAL_HOST): Identity[] {
  const want = host || LOCAL_HOST
  const hostless = (i: Identity) => !i.host
  const out: Identity[] = []
  const push = (i: Identity) => {
    if (!out.some((x) => x.name === i.name)) out.push(i)
  }
  for (const i of all) if (!hostless(i) && i.host === want) push(i)
  if (want === LOCAL_HOST) for (const i of all) if (hostless(i)) push(i)
  for (const st of Object.values(status)) {
    if (st.source !== 'shell') continue
    push({ name: st.name, kind: st.kind, env: st.config_dir ? { CLAUDE_CONFIG_DIR: st.config_dir } : {}, args: [] })
  }
  if (want !== LOCAL_HOST) {
    // 那台的身分狀態還沒到＝alias 還沒偵測過（daemon 的 `shell: None`）。
    const detected = Object.keys(status).length > 0
    for (const i of all) {
      if (!hostless(i)) continue
      if (!detected && SHELL_IDENTITY_NAMES.includes(i.name)) continue
      push(i)
    }
  }
  return out
}

/**
 * 停用的身份不出現在**任何**地方：額度條、快速新增、身份選單、側欄的身分計數（使用者 2026-09-16）。
 * 唯一的例外是環境設定那一頁自己——不然沒有地方可以把它按回來。
 */
export function enabledIdentities(disabled: string[], host: string, list: Identity[]): Identity[] {
  if (disabled.length === 0) return list
  return list.filter((i) => !disabled.includes(api.identityPrefKey(host, i.kind, i.name)))
}

/**
 * 參與「額度落點」（`quotaLookup`，誰認領裸 key）的身分清單：那台主機的全集扣掉停用的。
 * 頂端額度條與側欄**都**走這一支——以前頂端先濾停用、側欄沒濾，停用 cc0 之後頂端把裸 key 給了
 * `main` 畫紅燈，側欄的 `main` 卻查 `claude:main` 查不到、不反灰（第二輪 review M1）。
 */
export function quotaClaimantsOf(all: Identity[], status: IdentityStatusMap, disabled: string[], host: string): Identity[] {
  return enabledIdentities(disabled, host, identitiesOfHost(all, status, host))
}

export function quotaClaimants(state: StoreState, host: string): Identity[] {
  return quotaClaimantsOf(state.identities, identityStatusOfHost(state, host), state.disabledIdentities, host)
}

/** 這個身份在這台主機上被停用了嗎（停用是 host＋kind＋name 一組）。 */
export function identityDisabled(state: StoreState, host: string, kind: string, name: string): boolean {
  return state.disabledIdentities.includes(api.identityPrefKey(host, kind, name))
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

// 多分頁：別的分頁送出（清掉草稿）時，這個分頁的輸入框也要跟著清，不然同一句會被再送一次（#369）。
if (typeof window !== 'undefined' && typeof window.addEventListener === 'function' && !IN_MOBILE_PREVIEW) {
  window.addEventListener('storage', (ev: StorageEvent) => {
    if (ev.key !== DRAFTS_KEY) return
    const gone = draftsClearedElsewhere(useStore.getState().drafts, ev.oldValue, ev.newValue)
    for (const k of gone) useStore.setState((s) => ({ drafts: withoutKey(s.drafts, k), draftCursors: withoutKey(s.draftCursors, k) }))
  })
}
