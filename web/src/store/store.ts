import { receiveCodexReset } from './codexReset'
import { isApiKind } from '../components/kindMeta'
import { handleToolInstallDone } from '../lib/toolInstallDone'
import { autoInstallNotice } from './integrationAutoInstall'
import type { TerminalFooterView } from '../lib/codexTerminalFooter'

import { gatewayErrText, networkErrText } from '../lib/netErr'
import { DraftSync } from './draftSync'
import { resetAttachmentDraftsForTest } from './composerAttachmentDrafts'
import { useDraftNotice } from './draftNotice'
import { pruneDeadKeys } from './prune'
import { forgetBlockedExcept } from '../lib/blockedPrefetch'
import { shellDraftKey } from './shellDraft'
import { createResyncRunner } from './resyncQueue'
import { quotaLimitChanged } from './quotaLimitChange'
import { createRequestId, peekRequestId, settleCreateRequest } from '../lib/createRequestId'
import { queueSlotNotice, slotHolderFrom } from '../lib/queueSlot'
import { UNKNOWN_SENT_NOTICE, unknownLock } from './deliveryUnknown'
import { create } from 'zustand'
import { markUserNav } from './userNav'
import * as api from '../api'
import { retireApiSession } from '../api/apiBotSession'
import { toClaudeLoginExpiresAt } from '../api/normalize'
import {
  frameBotId,
  hostArray,
  lampOf,
  toKindQuota,
  toMemSnapshot,
  toIdentityStatusMap,
  toToolMap,
  toHerdrVersion,
  toHostBaseline,
  optStr,
  sortByTime,
  str,
  toMessage,
  toBotLimitHit,
  toRun,
  toTurn,
  unwrap,
  isRec,
  bool,
  num,
  pick,
  arr,
} from '../api/normalize'
import { ApiError } from '../api/types'
import { getTimeZone } from '../api/timeZone'
import { DEFAULT_USER_TIME_ZONE } from '../lib/timeZone'
import { DRAFT_REASON_TEXT, draftBlockFrom, draftBlockFromSnapshot, draftIsGone, draftIsStale, markDraftBusy, pruneStaleDrafts, type ComposerDraftBlock, type DraftRequest } from './composerDraft'
import { PREVIEW_OFF, toPreviewEvent, type Preview } from '../api/preview'
import type { Bot, BotKind, RestartBatch, MemSnapshot, Host, HerdrVersion, HostBaseline, HostResult, HostShell, Identity, IdentityStatusMap, Lamp, Message, ModelInfo, NewBotInput, NewHostInput, NewIdentityInput, NewProjectInput, PatchBotInput, PatchProjectInput, Project, QuotaMap, Run, TerminalFormat, TerminalSource, ToolMap, Turn, TurnDelivery, ModelRemap } from '../api/types'
import type { ProjectPane } from '../api'
import { joinRunningBatch, reconcileBatch, restartProgress } from './restartBatch'
import { cliUpdateDone, cliUpdateDoneMany, cliUpdateProgressMany, reconcileCliUpdates, type CliUpdate } from './cliUpdate'
import { applyHerdrSnapshot, onHerdrFrame } from './herdrUpdate'
import { applyDeployWaitSnapshot, onDeployWaitFrame } from './deployWait'
import { gateFrame } from './frameSeen'
import { applyUpstreamItem, loadUpstreamUpdates, type UpstreamItem } from './upstreamUpdate'
import { dropHostModels, modelsKey, saveLastModels, shouldFetchModels, type ModelsCache, type ModelsMetaCache } from './modelsCache'
import { byTime, capList, keptAfterPage, oldestByInsert, pruneTurns, reuseUnchanged, upsertSorted } from './lists'
import { recoverLostCursor } from './pageCursor'
import { type CapFloors, capFor, clearFloor, raiseFloor } from './messageCap'
import { markRewound } from '../lib/rewind'
import { acceptStateSeq, singleFlight } from './singleFlight'
import { bumpEventRevs } from './eventRevs'
import { quotaForIdentity, weeklyOnlyKind } from './quotaLookup'
import { botStatusConnTarget } from './botStatusConn'
import { paneReadOnly } from '../lib/shellAccess'
import { groupByProject, withPane, withoutPane } from '../lib/paneLists'
import { prependDraft, queuedSendFor } from './queuedSend'
import { noteQueuedTurn, startingSend, startingSendLabel } from './startingSend'
import { asUncommittedSend, noteInFlightTurn, uncommittedSendText } from './uncommittedSend'
import { suggestionFailureText } from './promptSuggestion'
import { dismissKey, pruneDismissed, type PendingLogin } from '../lib/loginPrompt'
import { codexLoginReplacesLogin } from '../lib/codexLoginGuard'
import { sendNowFellThrough } from './sendNowOutcome'
import { sendNowNotice } from './sendNowCopy'

export interface PatchBotOutcome {
  needsRestart: boolean
  deferred: boolean
  remappedModel: ModelRemap | null
  message?: string | null
}

export interface ApiModelConflict {
  botId: string
  checkpointModel: string
  currentModel: string
}
import { laterMark, serverUnread } from './sharedUnread'
import { isKeepWarmRequestId } from '../lib/keepWarm'
import { viewingBot } from './viewing'
import {
  botKey,
  clearHookCompletion,
  completesTurn,
  completionKey,
  countUnreadTurns,
  idleEdgeCompletionKey,
  isExpertBot,
  loadCounts,
  loadMarks,
  markHookCompletion,
  markNow,
  markOfMessages,
  pruneIdleEdges,
  prunePersistedMarks,
  pruneUnread,
  relayTurnIds,
  saveCounts,
  recordReadMark,
  takeTurnCompletion,
  windowActive,
  type ReadMark,
} from './unread'
import { fetchSupervisor } from '../api/supervisor'
import { writeShared } from './mobilePreview'
import { browserTabStores, readTabValue, writeTabValue } from '../lib/tabStorage'
import { BOT_KINDS, LOCAL_HOST, quotaKey } from '../api/types'
import { supervisorOwnedAsk, type AgmDeleteAsk } from '../lib/agmDelete'
import { handedOffReason, handedOffTo } from '../lib/handoff'
import { pruneStoredIds } from '../lib/storedIds'
import { msg, t, tr } from '../lib/i18n'
import { isLegacyDaemonError, plainErrMsg } from '../lib/errText'
import { apiBotStartReasonText } from '../lib/apiBotStartReason'
import { leftPct } from '../lib/quotaLeft'

const messagePageGenerations = new Map<string, number>()
let legacyQueueMigrationAttempted = false
let knownBotIds = new Set<string>()
const goneBotIds = new Set<string>()

function noteGoneBots(bots: Bot[]): void {
  const now = new Set(bots.map((b) => b.id))
  for (const id of knownBotIds) if (!now.has(id)) goneBotIds.add(id)
  for (const id of now) goneBotIds.delete(id)
  knownBotIds = now
}

function pruneStoreMemo(s: Pick<StoreState, 'bots'>): void {
  const botIds = new Set(s.bots.map((b) => b.id))
  for (const key of [...messagePageGenerations.keys()]) {
    const id = key.slice(key.indexOf(':') + 1)
    if (!botIds.has(id)) messagePageGenerations.delete(key)
  }
  pruneIdleEdges(botIds)
  forgetBlockedExcept(botIds)
}

export function storeMemoKeysForTest() {
  return {
    messagePageGenerations: [...messagePageGenerations.keys()],
  }
}

export function noteMessagePageForTest(kind: 'bot', id: string) {
  advanceMessagePageGeneration(kind, id)
}

function messagePageGeneration(kind: 'bot', id: string): number {
  return messagePageGenerations.get(`${kind}:${id}`) ?? 0
}

function advanceMessagePageGeneration(kind: 'bot', id: string): void {
  const key = `${kind}:${id}`
  messagePageGenerations.set(key, (messagePageGenerations.get(key) ?? 0) + 1)
}

export type SocketStatus = 'connecting' | 'open' | 'closed'
export type RightTab = 'chat' | 'terminal' | 'preview'

export type SettingsAnchor = { left: number; right: number; top: number; bottom: number }

export function anchorOf(el: Element): SettingsAnchor {
  const r = el.getBoundingClientRect()
  return { left: r.left, right: r.right, top: r.top, bottom: r.bottom }
}
export type KindDisplay = 'icon' | 'text'

const KIND_DISPLAY_KEY = 'am.kindDisplay'
const DRAFT_CURSORS_KEY = 'am.draftCursors'
const SELECTION_KEY = 'am.selection'

interface Selection {
  botId: string | null
  projectId: string | null
}

const NO_SELECTION: Selection = { botId: null, projectId: null }

function parseSelection(raw: string): Selection | null {
  const parsed: unknown = JSON.parse(raw)
  if (!isRec(parsed)) return null
  return {
    botId: optStr(pick(parsed, 'botId')),
    projectId: optStr(pick(parsed, 'projectId')),
  }
}

function readSelection(): Selection {
  return readTabValue(SELECTION_KEY, parseSelection, browserTabStores()) ?? NO_SELECTION
}

function writeSelection(sel: Selection) {
  writeShared(() => writeTabValue(SELECTION_KEY, JSON.stringify(sel), browserTabStores()))
}

const initialSelection = readSelection()

const SHELL_VIEW_KEY = 'am.shellView'

export type ShellView = {
  host: string
  paneId: string
  cwd: string
  readOnly?: boolean
  readOnlyReason?: string
  traced?: boolean
}

function parseShellView(raw: string): ShellView | null {
  const parsed: unknown = JSON.parse(raw)
  if (!isRec(parsed)) return null
  const host = optStr(pick(parsed, 'host'))
  const paneId = optStr(pick(parsed, 'paneId'))
  if (!host || !paneId) return null
  return {
    host,
    paneId,
    cwd: optStr(pick(parsed, 'cwd')) ?? '',
    readOnly: parsed.readOnly === true,
    traced: parsed.traced === true,
  }
}

function readShellView(): ShellView | null {
  return readTabValue(SHELL_VIEW_KEY, parseShellView, browserTabStores())
}

function writeShellView(v: ShellView | null) {
  writeShared(() => writeTabValue(SHELL_VIEW_KEY, v ? JSON.stringify(v) : null, browserTabStores()))
}

const initialShellView = readShellView()

export type DraftKey = `bot:${string}` | `shell:${string}`

export interface DraftCursor {
  start: number
  end: number
}

function persistDiff<V>(storageKey: string, prev: Record<string, V>, next: Record<string, V>, read: () => Record<string, V>) {
  const disk = read()
  for (const k of Object.keys(prev)) if (!(k in next)) delete disk[k]
  for (const [k, v] of Object.entries(next)) if (prev[k] !== v) disk[k] = v
  localStorage.setItem(storageKey, JSON.stringify(disk))
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
  }
}

function readKindDisplay(): KindDisplay {
  try {
    return localStorage.getItem(KIND_DISPLAY_KEY) === 'text' ? 'text' : 'icon'
  } catch {
    return 'icon'
  }
}

const initialUnread = loadCounts()
let readMarks = loadMarks()

function setReadMark(key: string, mark: ReadMark) {
  readMarks = recordReadMark(readMarks, key, mark)
}

function persistUnread(s: { botUnread: Record<string, number> }) {
  saveCounts({ bots: s.botUnread })
}

export interface Notice {
  id: number
  kind: 'error' | 'warn' | 'info'
  text: string
  topCenter?: boolean
  action?: { label: string; run: () => void | Promise<void> }
}

const LIVE_THROTTLE_MS = 250
type LiveThrottle = { turnId: string; apply: null | (() => void); timer: ReturnType<typeof setTimeout> }
const liveThrottle = new Map<string, LiveThrottle>()

function clearLiveThrottle(botId: string, turnId?: string): void {
  const slot = liveThrottle.get(botId)
  if (!slot || (turnId !== undefined && slot.turnId !== turnId)) return
  clearTimeout(slot.timer)
  liveThrottle.delete(botId)
}

const PAGE_SIZE = 200

export interface LiveReply {
  turnId: string
  text: string
  activity: string
  alert: string
  revision: number
  estCostMicrousd: number | null
}

export type { QueuedSend } from './queuedSend'

export interface ComposerState {
  disabled: boolean
  reason: string
  queued: boolean
  inFlightTurnId: string | null
  unknownTurnId: string | null
  unknownWorking?: boolean
  autoStart?: boolean
}

export interface ShellIdentityDeleteRef {
  type: string
  id?: string
}

export interface ShellIdentityDeleteOutcome {
  ok: boolean
  backup_path?: string
  reason?: string
  message?: string
  refs?: ShellIdentityDeleteRef[]
}

export interface StoreState {
  ready: boolean
  bootError: string | null
  socket: SocketStatus
  connected: boolean
  defaultConnected: boolean
  lastSeq: number
  lastDurableSeq: number
  stateStale: boolean

  hosts: Host[]
  attachCommand: string
  localTools: ToolMap
  localHerdr: HerdrVersion
  localBaseline: HostBaseline | null
  hostBaselineSync: Record<string, 'off' | 'ask' | 'auto'>
  localIdentityStatus: IdentityStatusMap
  localClaudeLoginExpiresAt: Record<string, number>
  dismissedLoginExpiry: Record<string, string>
  disabledIdentities: string[]
  disabledKinds: string[]
  setKindDisabled: (host: string, kind: string, disabled: boolean) => Promise<void>
  mem: MemSnapshot | null
  quota: QuotaMap
  ledgerRev: Record<string, number>
  ledgerAnyRev: number
  models: ModelsCache
  modelsMeta: ModelsMetaCache
  modelsFailedAt: Record<string, number>
  dropHostModels: (host: string) => void
  kindDisplay: KindDisplay
  toolHintDismissed: boolean
  drafts: Record<string, string>
  draftCursors: Record<string, DraftCursor>
  identities: Identity[]
  projects: Project[]
  bots: Bot[]
  terminalFooter: TerminalFooterView | null
  runs: Record<string, Run | null>
  turns: Record<string, Record<string, Turn>>
  messages: Record<string, Message[]>
  loadedBots: Record<string, boolean>
  messageLoadFailed: Record<string, true>
  moreMessages: Record<string, boolean>
  messageCapFloors: CapFloors
  loadingMore: Record<string, boolean>
  botUnread: Record<string, number>
  hiddenBotIds: string[]
  liveReply: Record<string, LiveReply>

  selectedProjectId: string | null

  supervisorProjectId: string | null
  loadSupervisorProject: () => Promise<void>

  supervisorRev: number
  schedulesRev: number

  shellView: ShellView | null

  selectedBotId: string | null
  rightTab: RightTab
  previews: Record<string, Preview>
  setPreview: (botId: string, p: Preview) => void
  shareRev: Record<string, number>
  settingsBotId: string | null
  settingsAnchor: SettingsAnchor | null
  botOrder: Record<string, string[]>
  projectOrder: string[]
  openBotSheetFor: string | null
  notices: Notice[]
  busy: Record<string, boolean>

  bootstrap: () => Promise<void>
  refreshState: () => Promise<void>
  selectBot: (botId: string | null) => void
  selectAdjacentBot: (dir: -1 | 1) => void
  selectProject: (projectId: string | null) => void
  setRightTab: (tab: RightTab) => void
  openSettings: (botId: string, anchor?: SettingsAnchor | null) => void
  moveBot: (botId: string, beforeId: string | null) => void
  movePrimary: (order: string[]) => void
  moveProject: (projectId: string, beforeId: string | null) => void
  closeSettings: () => void
  requestOpenBotSheet: (projectId: string) => void
  clearOpenBotSheet: () => void
  loadMessages: (botId: string) => Promise<void>
  loadEarlierMessages: (botId: string) => Promise<void>
  markBotRead: (botId: string) => void
  markCurrentRead: () => void
  recountBot: (botId: string) => void
  pruneUnread: () => void
  pruneDead: () => void
  notify: (kind: Notice['kind'], text: string, action?: Notice['action'], topCenter?: boolean) => void
  dismiss: (id: number) => void
  dismissLoginExpiry: (key: string, stage: string) => void

  startBot: (botId: string) => Promise<void>
  stopBot: (botId: string) => Promise<void>
  interruptBot: (botId: string) => Promise<void>
  abortBot: (botId: string) => Promise<boolean>
  loginBot: (botId: string) => Promise<boolean>
  sendPrompt: (
    botId: string,
    text: string,
    attachments?: string[],
    sendNow?: boolean,
    startIfStopped?: boolean,
    draft?: DraftRequest,
    queueIfBusy?: boolean,
    cacheTtl?: '5m' | '1h',
  ) => Promise<boolean>
  composerDrafts: Record<string, ComposerDraftBlock>
  dismissComposerDraft: (botId: string) => void
  loadComposerDraft: (botId: string, opts?: { editNow?: boolean; tries?: number; delayMs?: number }) => Promise<ComposerDraftBlock | null>
  setComposerEditing: (botId: string, flags: { editing?: boolean; editNow?: boolean }) => void
  loginPromptDismissed: Record<string, string>
  dismissLoginPrompt: (host: string, identity: string, episode: string) => void
  codexLoginAsk: { resolve: (ok: boolean) => void } | null
  confirmCodexLoginReplace: (host: string, kind: BotKind, identity: string | null) => Promise<boolean>
  answerCodexLoginAsk: (ok: boolean) => void
  pruneLoginPromptDismissed: (pending: PendingLogin[]) => void
  apiModelConflict: ApiModelConflict | null
  setApiModelConflict: (conflict: ApiModelConflict | null) => void
  resolveApiModelConflict: (action: 'revert' | 'clear') => Promise<void>
  acceptSuggestion: (botId: string) => Promise<boolean>
  sendKeys: (botId: string, keys: string[], pinRunId?: string | null) => Promise<void>
  sendText: (botId: string, text: string, enter: boolean, record?: boolean, pinRunId?: string | null) => Promise<boolean>
  abandonTurn: (botId: string, turnId: string) => Promise<void>
  addHost: (input: NewHostInput) => Promise<HostResult | null>
  addIdentity: (input: NewIdentityInput) => Promise<boolean>
  removeIdentity: (name: string, host?: string) => Promise<void>
  removeShellIdentity: (host: string, identity: string) => Promise<ShellIdentityDeleteOutcome>
  removeHost: (name: string) => Promise<void>
  reconnectHost: (name: string) => Promise<HostResult | null>
  addProject: (input: NewProjectInput) => Promise<boolean>
  addBot: (projectId: string, input: NewBotInput) => Promise<string | null>
  cloneBot: (botId: string) => Promise<string | null>
  forkBot: (botId: string) => Promise<string | null>
  promoteBot: (botId: string) => Promise<string | null>
  patchBot: (botId: string, input: PatchBotInput) => Promise<PatchBotOutcome | null>
  patchProject: (projectId: string, input: PatchProjectInput) => Promise<boolean>
  restartBot: (botId: string, resumeNative?: boolean) => Promise<boolean>
  restartIdleBots: () => Promise<void>
  updateRestartScheduled: string[]
  scheduleUpdateRestart: (botIds: string[]) => Promise<void>
  cancelUpdateRestart: (botIds: string[]) => Promise<void>
  restartBatch: RestartBatch | null
  clearRestartBatch: () => void
  cliUpdates: CliUpdate[]
  upstreamUpdates: Record<string, UpstreamItem>
  startCliUpdate: (host: string, kind: 'codex' | 'claude', targetVersion: string) => Promise<void>
  installCodexUpdate: (host: string, targetVersion: string) => Promise<void>
  installClaudeUpdates: (hosts: string[], targetVersion: string) => Promise<void>
  removeBot: (botId: string, opts?: { confirmSupervisor?: boolean }) => Promise<void>
  restoreBot: (botId: string) => Promise<boolean>
  agmDeleteAsk: AgmDeleteAsk | null
  cancelAgmDelete: () => void
  removeProject: (projectId: string) => Promise<void>
  readTerminal: (botId: string, source: TerminalSource, lines: number, format?: TerminalFormat) => ReturnType<typeof api.fetchTerminal>

  openHostShell: (host: string, cwd?: string) => Promise<boolean>
  viewHostShell: (shell: HostShell) => void
  sidePanes: Record<string, ProjectPane[]>
  unownedPanes: ProjectPane[]
  refreshPanes: () => Promise<void>
  closeTracedPane: (pane: ProjectPane, confirm: boolean) => Promise<'closed' | ProjectPane | null>
  paneGone: (host: string, paneId: string) => void
  viewPane: (pane: ProjectPane) => void
  closeShellView: () => void
  lockShellView: (host: string, paneId: string, reason: string) => void
  restoreShellView: () => Promise<void>
  endHostShell: (host: string, paneId: string, confirm?: boolean) => Promise<api.CloseNeedsConfirm | null>

  loadQuota: () => Promise<void>
  loadMem: () => Promise<void>
  userTimeZone: string
  loadUserTimeZone: () => Promise<void>
  setUserTimeZone: (zone: string) => void
  loadModels: (kind: BotKind, host: string, identity?: string | null) => Promise<ModelInfo[] | null>
  installTool: (host: string, kind: BotKind, viaBotId: string) => Promise<string | null>
  loginIdentity: (host: string, identity: string, opts?: { background?: boolean }) => Promise<boolean>
  identityLoginPane: { host: string; identity: string; paneId: string; cwd: string } | null
  openIdentityLoginPane: () => void
  logoutIdentity: (host: string, identity: string) => Promise<boolean>
  logoutAgy: (host: string) => Promise<boolean>
  logoutKindDefault: (host: string, kind: BotKind) => Promise<boolean>
  installAgy: (host: string) => Promise<boolean>
  loadIdentityPrefs: () => Promise<void>
  setIdentityDisabled: (host: string, kind: string, name: string, disabled: boolean) => Promise<void>
  refreshTools: (host: string) => Promise<boolean>
  setKindDisplay: (mode: KindDisplay) => void
  dismissToolHint: () => void
  setDraft: (key: DraftKey, text: string) => void
  setDraftCursor: (key: DraftKey, start: number, end?: number) => void

  cancelStartingSend: (botId: string) => Promise<void>
  withdrawQueuedSend: (botId: string) => Promise<void>
}

let noticeSeq = 0
const noticeTimers = new Map<number, ReturnType<typeof setTimeout>>()

function forkErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (isLegacyDaemonError(e)) {
      return t('fork 失敗：正在跑的 daemon 還沒有 fork 功能，要重新 build 並重啟 daemon 後才能用。')
    }
    if (typeof e.body.message === 'string' && e.body.message) return t('fork 失敗：{message}', { message: e.body.message })
  }
  return t('fork 失敗：{reason}', { reason: errText(e) })
}

const PROMOTE_REASON_TEXT: Record<string, string> = {
  session_not_found: msg('找不到它的 claude session（pane 裡沒有對得上的行程或對話檔）'),
  session_ambiguous: msg('pane 裡有不只一段 claude session，無法確定要接哪一段'),
  transcript_exists: msg('目標目錄已經有一份不同內容的同名對話檔，不覆寫'),
  has_children: msg('它自己還有子 agent，先處理掉再升級'),
  stop_failed: msg('停不掉這顆子 agent，什麼都沒動'),
  remote_not_supported: msg('遠端主機上的子 agent 還不支援升級'),
  unsupported_kind: msg('只有 claude 的子 agent 能升級'),
  promote_start_failed: msg('升級後的新 bot 起不來，已收回'),
}

function promoteErrText(e: unknown): string {
  if (e instanceof ApiError) {
    if (isLegacyDaemonError(e)) {
      return t('升級失敗：正在跑的 daemon 還沒有這個功能，要重新 build 並重啟 daemon 後才能用。')
    }
    const why = typeof e.body.reason === 'string' ? PROMOTE_REASON_TEXT[e.body.reason] : undefined
    if (why) return t('升級失敗：{why}', { why: tr(why) })
    if (typeof e.body.message === 'string' && e.body.message) return t('升級失敗：{message}', { message: e.body.message })
  }
  return t('升級失敗：{reason}', { reason: errText(e) })
}

const REASON_TEXT: Record<string, string> = {
  composer_unreadable: msg('沒送出：讀不到 bot 的輸入框（可能正在切換畫面或還在啟動），稍後再送一次'),
  composer_busy: msg('沒送出：bot 的輸入框裡有字（可能有人正在終端打字，或上一次插隊沒送出的字還留著），清掉或送出之後再送一次'),
  transcript_not_ready: msg('沒送出：claude 還沒回報這段對話（session），稍後再送一次'),
  transcript_unreadable: msg('沒送出：讀不到這段對話的紀錄檔，稍後再送一次'),
  codex_log_not_ready: msg('沒送出：codex 的紀錄還沒寫出來，稍後再送一次'),
  codex_security_banner: msg('沒送出：codex 畫面上有帳號安全提醒橫幅（開著時打字，開頭的數字會被當成選項）。請到「終端」選一項或按 Esc 關掉，再送一次'),
  no_pane_to_type_into: msg('沒送出：找不到這顆 bot 的終端畫面可以打字，重啟它再試'),
  resume_unverified: msg('沒送出：這顆 bot 是接回舊對話起來的，還在確認接回的是不是原本那段（最多約兩分鐘），稍後再送一次'),
  host_unreadable: msg('沒送出：讀不到這顆 bot 在哪台主機（daemon 的資料暫時讀不到），稍後再送一次'),
  ...DRAFT_REASON_TEXT,
}

const REASON_CODE_TEXT: Record<string, string> = {
  prompt_too_long_to_prove: msg('這則訊息超過 20 萬字，無法確認對方收到，請縮短或分段再送'),
  track_required: msg('專家必須填負責任務（track），空白不能儲存'),
  user_only: msg('只有使用者能改這個設定'),
}

function reasonText(e: ApiError): string {
  const known = REASON_TEXT[e.message]
  if (known) return tr(known)
  const code = typeof e.body.reason === 'string' ? e.body.reason : typeof e.body.error === 'string' ? e.body.error : undefined
  const byCode = code ? REASON_CODE_TEXT[code] : undefined
  if (byCode) return tr(byCode)
  const human = e.body.message
  if (typeof e.body.reason === 'string' && typeof human === 'string' && human.trim()) return human.trim()
  return e.message
}

export async function sendWithFreshRun(get: () => StoreState, botId: string, send: (runId: string | null) => Promise<unknown>): Promise<void> {
  try {
    await send(get().runs[botId]?.id ?? null)
  } catch (e) {
    const fresh = e instanceof ApiError && e.status === 409 && e.message === 'run mismatch' ? e.body.run_id : undefined
    if (typeof fresh !== 'string' || !fresh) throw e
    await send(fresh)
    void get().refreshState()
  }
}

export const errTextForTest = (e: unknown): string => errText(e)

function errText(e: unknown): string {
  const net = networkErrText(e)
  if (net) return net
  if (e instanceof ApiError) {
    const human = typeof e.body.message === 'string' && e.body.message.trim() !== ''
    const gw = gatewayErrText(e.status, human || typeof e.body.error === 'string')
    if (gw) return gw
    return t('{reason}（HTTP {status}）', { reason: reasonText(e), status: e.status })
  }
  return plainErrMsg(e)
}

function startErrText(e: unknown, kind?: unknown): string {
  if (e instanceof ApiError) {
    const apiBot = apiBotStartReasonText(e.body.reason, kind) ?? apiBotStartReasonText(e.body.message, kind) ?? apiBotStartReasonText(e.body.error, kind)
    if (apiBot) return apiBot
  }
  if (e instanceof ApiError && e.body.reason === 'identity_not_logged_in' && typeof e.body.hint === 'string' && e.body.hint) {
    return e.body.hint
  }
  return errText(e)
}

function isRunStateUncommitted(e: unknown, which: 'start' | 'stop' | 'restart'): boolean {
  return e instanceof ApiError && e.status === 503 && e.body.error === `${which}_state_uncommitted`
}

function isActiveRunConflict(e: unknown): boolean {
  return e instanceof ApiError && e.status === 409 && (e.body.reason === 'active run already exists' || e.body.reason === 'api_run_already_active')
}

function runStateUncommittedText(e: unknown): string {
  if (e instanceof ApiError && typeof e.body.message === 'string' && e.body.message.trim()) return e.body.message.trim()
  return errText(e)
}

function loginErrText(e: unknown): string {
  if (!(e instanceof ApiError)) return errText(e)
  const kind = typeof e.body.kind === 'string' ? e.body.kind : t('這個 agent')
  switch (e.body.error === 'login_unsupported' ? 'login_unsupported' : e.body.reason) {
    case 'login_unsupported':
      return t('{kind} 的 TUI 沒有登入指令，只能到它跑的那台主機上執行 `{kind} login`。', { kind })
    case 'not_running':
      return t('這個 bot 沒在跑。先啟動它，再按登入。')
    case 'agent_busy':
      return t('agent 正在忙，這時候打字會被吃掉。等它停下來再按。')
    case 'turn_in_flight':
      return t('有回合還在進行中，登入指令會被當成那個提問的一部分。等這回合結束再按。')
    case 'no_pane':
      return t('找不到這個 bot 的終端機畫面，可能已經被關掉了。重啟這個 bot 再試。')
    default:
      return errText(e)
  }
}

let appliedStateSeq = 0
let supervisorProjectAsked = false
let identityPrefsAsked = false
let identityPrefsEpoch = 0
let identityPrefsLoadSeq = 0
let lastRefreshError: string | null = null
const unsentReads = new Map<string, ReadMark>()

function sendReadMark(botId: string, mark: ReadMark) {
  unsentReads.set(botId, mark)
  void api
    .markBotRead(botId, mark)
    .then(() => {
      if (unsentReads.get(botId) === mark) unsentReads.delete(botId)
    })
    .catch(() => {})
}

function flushUnsentReads() {
  for (const [botId, mark] of [...unsentReads]) sendReadMark(botId, mark)
}

let orderSavesInFlight = 0
const orderSaveGen = new Map<string, number>()
const orderSaveTail = new Map<string, Promise<void>>()
const ORDER_SAVE_TIMEOUT_MS = 15_000
const orderSaveTimeout = () => AbortSignal.timeout(ORDER_SAVE_TIMEOUT_MS)
let orderSaveSignal: () => AbortSignal = orderSaveTimeout
export function setOrderSaveSignalForTest(make: (() => AbortSignal) | null) {
  orderSaveSignal = make ?? orderSaveTimeout
}
function saveOrderTracked(key: string, input: Parameters<typeof api.saveOrder>[0], onFail: (e?: unknown) => void) {
  orderSavesInFlight += 1
  const gen = (orderSaveGen.get(key) ?? 0) + 1
  orderSaveGen.set(key, gen)
  const send = () => api.saveOrder(input, orderSaveSignal())
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

function resetStateSeq() {
  appliedStateSeq = 0
}

type SetFn = (partial: Partial<StoreState> | ((s: StoreState) => Partial<StoreState>)) => void
type GetFn = () => StoreState
let boundSet: SetFn | null = null
let boundGet: GetFn | null = null

const seenScheduleNotices = new Set<string>()
const seenHerdrRecoveryNotices = new Set<string>()

const prevStoreState = import.meta.hot?.data?.storeState as Record<string, unknown> | undefined
const initialStoreData: Record<string, unknown> = {}
if (prevStoreState) {
  for (const [k, v] of Object.entries(prevStoreState)) {
    if (typeof v !== 'function') {
      initialStoreData[k] = v
    }
  }
}

export const useStore = create<StoreState>((set, get) => {
  boundSet = set
  boundGet = get
  return {
  ready: false,
  userTimeZone: DEFAULT_USER_TIME_ZONE,
  bootError: null,
  socket: 'connecting',
  connected: true,
  defaultConnected: false,
  lastSeq: 0,
  lastDurableSeq: 0,
  stateStale: false,

  hosts: [],
  attachCommand: 'herdr --session agents-manager',
  localTools: toToolMap(undefined),
  localHerdr: toHerdrVersion(undefined),
  localBaseline: null,
  hostBaselineSync: {},
  localIdentityStatus: {},
  localClaudeLoginExpiresAt: {},
  dismissedLoginExpiry: {},
  disabledIdentities: [],
  disabledKinds: [],
  quota: {},
  ledgerRev: {},
  ledgerAnyRev: 0,
  mem: null,
  models: {},
  modelsMeta: {},
  modelsFailedAt: {},
  kindDisplay: readKindDisplay(),
  toolHintDismissed: false,
  drafts: {},
  draftCursors: readDraftCursors(),
  identities: [],
  projects: [],
  bots: [],
  terminalFooter: null,
  runs: {},
  turns: {},
  messages: {},
  loadedBots: {},
  messageLoadFailed: {},
  moreMessages: {},
  messageCapFloors: {},
  loadingMore: {},
  botUnread: initialUnread.bots,
  hiddenBotIds: [],
  liveReply: {},
  composerDrafts: {},
  loginPromptDismissed: {},
  codexLoginAsk: null,
  apiModelConflict: null,

  selectedProjectId: initialSelection.projectId,

  supervisorRev: 0,
  schedulesRev: 0,
  shellView: initialShellView,
  identityLoginPane: null,
  supervisorProjectId: null,
  agmDeleteAsk: null,

  selectedBotId: initialSelection.botId,
  rightTab: 'chat',
  previews: {},
  setPreview: (botId, p) => set((s) => ({ previews: { ...s.previews, [botId]: p } })),
  shareRev: {},
  settingsBotId: null,
  settingsAnchor: null,
  botOrder: {},
  projectOrder: [],
  openBotSheetFor: null,
  notices: [],
  busy: {},
  restartBatch: null,
  updateRestartScheduled: [],
  cliUpdates: [],
  upstreamUpdates: {},

  notify: (kind, text, action, topCenter) => {
    if (!action && get().notices.some((n) => n.kind === kind && n.text === text)) return
    noticeSeq += 1
    const id = noticeSeq
    set((s) => ({ notices: [...s.notices, { id, kind, text, action, topCenter }] }))
    const timer = setTimeout(() => {
      noticeTimers.delete(id)
      get().dismiss(id)
    }, action ? 15000 : kind === 'error' ? 8000 : 4000)
    noticeTimers.set(id, timer)
  },

  dismiss: (id) => {
    const timer = noticeTimers.get(id)
    if (timer !== undefined) {
      clearTimeout(timer)
      noticeTimers.delete(id)
    }
    set((s) => ({ notices: s.notices.filter((n) => n.id !== id) }))
  },

  dismissLoginExpiry: (key, stage) => {
    set((s) => ({
      dismissedLoginExpiry: { ...s.dismissedLoginExpiry, [key]: stage },
    }))
  },

  async bootstrap() {
    try {
      await api.session()
      await migrateLegacyQueuedSends(get)
      await get().refreshState()
      if (get().stateStale) throw new Error(lastRefreshError ?? t('無法讀取 daemon 狀態'))
      await get().restoreShellView()
      set({ ready: true, bootError: null })
    } catch (e) {
      set({ ready: false, bootError: errText(e) })
      return
    }
    connectSocket(set, get)
    reloadUpstreamUpdates(set, get)
    void get().loadQuota()
    void get().loadMem()
    void get().loadUserTimeZone()
    void draftSync.load().catch(() => {})
    if (!quotaSweep) quotaSweep = setInterval(() => void get().loadQuota(), QUOTA_SWEEP_MS)
  },

  refreshState: singleFlight(async () => {
    const durableAtStart = get().lastDurableSeq
    const st = await api.fetchState()
    const seq = acceptStateSeq(appliedStateSeq, st.daemon_seq)
    if (seq === null) {
      lastRefreshError = null
      set({ stateStale: false })
      return
    }
    if (st.projects.length === 0 && get().projects.length > 0 && get().socket !== 'open') return
    lastRefreshError = null
    set({ stateStale: false })
    appliedStateSeq = seq
    const behindFrames = get().lastDurableSeq > st.daemon_seq
    const runs: Record<string, Run | null> = {}
    for (const b of st.bots) runs[b.id] = st.runs.find((r) => r.bot_id === b.id) ?? null
    set((s) => {
      const turns = { ...s.turns }
      const listed = new Set(st.turns.map((t) => t.id))
      const inSnapshot = new Set(st.bots.map((b) => b.id))
      for (const [botId, map] of Object.entries(turns)) {
        const runId = runs[botId]?.id
        turns[botId] = Object.fromEntries(
          Object.entries(map).filter(([id, t]) => {
            if (t.status === 'in_flight' && t.run_id !== runId) return false
            if (!behindFrames && t.status === 'queued' && inSnapshot.has(botId) && !listed.has(id)) return false
            return true
          }),
        )
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
      const keptPending: Bot[] = []
      let botOrder = s.botOrder
      for (const b of s.bots) {
        if (!b.pending) continue
        const real = b.create_request_id ? st.bots.find((r) => r.create_request_id === b.create_request_id) : undefined
        if (!real) {
          keptPending.push(b)
          continue
        }
        const order = (botOrder[b.project_id] ?? []).map((x) => (x === b.id ? real.id : x))
        botOrder = { ...botOrder, [b.project_id]: order }
      }
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
        localBaseline: st.baseline,
        hostBaselineSync: st.host_baseline_sync ?? {},
        localIdentityStatus: st.identity_status,
        localClaudeLoginExpiresAt: st.claude_login_expires_at ?? {},
        identities: st.identities,
        projects: st.projects,
        bots: [...st.bots, ...keptPending],
        botOrder,
        projectOrder,
        runs,
        turns,
        connected: st.connected,
        defaultConnected: st.default_connected,
        lastSeq: (() => {
          const seen = Math.max(s.lastSeq, ephemeralSeq)
          ephemeralSeq = 0
          return st.daemon_seq < seen ? st.daemon_seq : Math.max(seen, st.daemon_seq)
        })(),
        lastDurableSeq: st.daemon_seq < durableAtStart ? st.daemon_seq : Math.max(s.lastDurableSeq, st.daemon_seq),
        restartBatch: reconcileBatch(s.restartBatch, st.restart_batch),
        cliUpdates: reconcileCliUpdates(s.cliUpdates, st.cli_updates),
        selectedBotId: selected,
        selectedProjectId: selectedProject,
      }
    })
    receiveCodexReset(st.codex_reset, get().notify)
    void api.fetchUpdateRestart().then((ids) => set({ updateRestartScheduled: ids }), () => {})
    applyHerdrSnapshot(st.herdr_updates)
    applyDeployWaitSnapshot(st.deploy_wait)
    {
      const s = get()
      const next = serverUnread(s.bots, s.botUnread, (id) => viewingBot(s, id) && windowActive(), unsentReads)
      if (next) {
        set({ botUnread: next })
        persistUnread(get())
      }
    }
    get().pruneUnread()
    get().pruneDead()
    if (behindFrames) void get().refreshState()
    const sel = get().selectedBotId
    if (sel && !get().loadedBots[sel]) await get().loadMessages(sel)
    if (get().supervisorProjectId === null) void get().loadSupervisorProject()
    if (!identityPrefsAsked) {
      identityPrefsAsked = true
      void get().loadIdentityPrefs()
    }
  }, (e) => reportStateRefreshError(set, get, e)),

  async loadSupervisorProject() {
    if (supervisorProjectAsked) return
    supervisorProjectAsked = true
    try {
      const info = await fetchSupervisor()
      if (info?.project_id) set({ supervisorProjectId: info.project_id })
    } catch {
      supervisorProjectAsked = false
    }
  },

  selectBot: (botId) => {
    markUserNav()
    set({ selectedBotId: botId, selectedProjectId: null, shellView: null, rightTab: 'chat', settingsBotId: null })
    if (botId && windowActive()) get().markBotRead(botId)
    if (botId && !get().loadedBots[botId]) void get().loadMessages(botId)
    if (botId) probeComposerDraftOnce(get, botId)
  },

  selectAdjacentBot: (dir) => {
    const next = adjacentBotId(get(), get().selectedBotId, dir)
    if (next) get().selectBot(next)
  },

  selectProject: (projectId) => {
    markUserNav()
    set({
      selectedProjectId: projectId,
      shellView: null,
      rightTab: 'chat',
      settingsBotId: null,
    })
  },

  setRightTab: (rightTab) => {
    markUserNav()
    set({ rightTab, settingsBotId: null })
  },

  openSettings: (botId, anchor = null) => {
    markUserNav()
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
      saveOrderTracked(`bots:${pid}`, { bots: { [pid]: next } }, () => {
        set((st) => ({ botOrder: prev ? { ...st.botOrder, [pid]: prev } : withoutKey(st.botOrder, pid) }))
        get().notify('error', t('排序沒存起來（daemon 沒收到），已回到原本的順序'))
      })
      return { botOrder }
    })
  },

  movePrimary: (order) => {
    const s = get()
    const prev = new Map(s.bots.map((b) => [b.id, b.primary_position]))
    const pos = new Map(order.map((id, i) => [id, i + 1]))
    if (order.every((id) => prev.get(id) === pos.get(id))) return
    set({ bots: s.bots.map((b) => (pos.has(b.id) ? { ...b, primary_position: pos.get(b.id)! } : b)) })
    saveOrderTracked('primary', { primary: order }, (e) => {
      set((st) => ({ bots: st.bots.map((b) => (prev.has(b.id) && pos.has(b.id) ? { ...b, primary_position: prev.get(b.id)! } : b)) }))
      get().notify('error', t('主力順序沒存起來，已回到原本的順序：{reason}', { reason: errText(e) }))
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
        get().notify('error', t('排序沒存起來（daemon 沒收到），已回到原本的順序'))
      })
      return { projectOrder: next }
    })
  },

  requestOpenBotSheet: (projectId) => set({ openBotSheetFor: projectId }),
  clearOpenBotSheet: () => set({ openBotSheetFor: null }),

  async loadMessages(botId) {
    if (get().messageLoadFailed[botId]) set((s) => ({ messageLoadFailed: withoutKey(s.messageLoadFailed, botId) }))
    advanceMessagePageGeneration('bot', botId)
    const generation = messagePageGeneration('bot', botId)
    try {
      const startedAt = new Date().toISOString()
      const page = await api.fetchMessages(botId)
      if (generation !== messagePageGeneration('bot', botId)) return
      advanceMessagePageGeneration('bot', botId)
      set((s) => {
        const kept = keptAfterPage(s.messages[botId] ?? [], page.messages, startedAt)
        const turns: Record<string, Turn> = {}
        for (const t of Object.values(s.turns[botId] ?? {})) {
          if (turnStillOpen(t.status) && t.created_at >= startedAt) turns[t.id] = t
        }
        for (const t of page.turns) {
          const have = s.turns[botId]?.[t.id]
          if (have && !turnStillOpen(have.status) && turnStillOpen(t.status)) {
            turns[t.id] = have
            continue
          }
          turns[t.id] = t
        }
        return {
          messages: { ...s.messages, [botId]: reuseUnchanged(s.messages[botId] ?? [], kept.length > 0 ? sortByTime([...page.messages, ...kept]) : page.messages) },
          turns: { ...s.turns, [botId]: turns },
          loadedBots: { ...s.loadedBots, [botId]: true },
          messageLoadFailed: withoutKey(s.messageLoadFailed, botId),
          moreMessages: { ...s.moreMessages, [botId]: page.has_more },
          messageCapFloors: clearFloor(s.messageCapFloors, botId),
        }
      })
      get().recountBot(botId)
    } catch (e) {
      if (generation !== messagePageGeneration('bot', botId)) return
      if (!get().loadedBots[botId]) set((s) => ({ messageLoadFailed: { ...s.messageLoadFailed, [botId]: true } }))
      get().notify('error', t('載入訊息失敗：{reason}', { reason: errText(e) }))
    }
  },

  async loadEarlierMessages(botId) {
    const s0 = get()
    const oldest = oldestByInsert(s0.messages[botId] ?? [])
    if (!oldest || s0.loadingMore[botId]) return
    const generation = messagePageGeneration('bot', botId)
    let lost: unknown
    set((s) => ({ loadingMore: { ...s.loadingMore, [botId]: true } }))
    try {
      const page = await api.fetchMessages(botId, PAGE_SIZE, oldest.id)
      set((s) => {
        if (generation !== messagePageGeneration('bot', botId)) return {}
        const have = new Set((s.messages[botId] ?? []).map((m) => m.id))
        const older = page.messages.filter((m) => !have.has(m.id))
        const merged = sortByTime([...older, ...(s.messages[botId] ?? [])])
        return {
          messages: { ...s.messages, [botId]: merged },
          moreMessages: { ...s.moreMessages, [botId]: page.has_more && older.length > 0 },
          messageCapFloors: raiseFloor(s.messageCapFloors, botId, older.length, merged.length),
        }
      })
    } catch (e) {
      lost = e
    } finally {
      set((s) => ({ loadingMore: withoutKey(s.loadingMore, botId) }))
    }
    if (lost === undefined) return
    const recovered = await recoverLostCursor(`bot:${botId}`, lost, () => get().loadMessages(botId), () => get().loadEarlierMessages(botId))
    if (!recovered) get().notify('error', t('載入更早的訊息失敗：{reason}', { reason: errText(lost) }))
  },

  markBotRead: (botId) => {
    const mark = markOfMessages(get().messages[botId] ?? []) ?? markNow()
    setReadMark(botKey(botId), mark)
    if (!botId.startsWith('pending:')) sendReadMark(botId, mark)
    if (!get().botUnread[botId]) return
    set((s) => ({ botUnread: withoutKey(s.botUnread, botId) }))
    persistUnread(get())
  },

  markCurrentRead: () => {
    if (!windowActive()) return
    const s = get()
    if (s.shellView) return
    if (s.selectedProjectId) return
    if (s.selectedBotId) get().markBotRead(s.selectedBotId)
  },

  pruneUnread: () => {
    const s = get()
    const liveBot = (id: string) => s.bots.some((b) => b.id === id)
    readMarks = prunePersistedMarks(readMarks, liveBot)
    const botUnread = pruneUnread(s.botUnread, liveBot)
    if (Object.keys(botUnread).length === Object.keys(s.botUnread).length) return
    set({ botUnread })
    persistUnread(get())
  },

  pruneDead: () => {
    const s = get()
    noteGoneBots(s.bots)
    pruneStoreMemo(s)
    pruneStoredIds({ botIds: s.bots.map((b) => b.id), projectIds: s.projects.map((p) => p.id) })
    const patch = pruneDeadKeys(s)
    if (!patch) return
    if (patch.drafts) for (const k of Object.keys(s.drafts)) if (!(k in patch.drafts)) draftSync.forget(k)
    if (patch.draftCursors) writeDraftCursors(s.draftCursors, patch.draftCursors)
    set(patch)
  },

  recountBot: (botId) => {
    const s = get()
    if (viewingBot(s, botId) && windowActive()) {
      get().markBotRead(botId)
      return
    }
    const serverMark = s.bots.find((b) => b.id === botId)?.read_mark
    const expert = isExpertBot(s.bots.find((b) => b.id === botId), s.projects)
    const n = countUnreadTurns(s.messages[botId] ?? [], laterMark(readMarks[botKey(botId)], serverMark), expert)
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
          if (e instanceof ApiError && e.status === 409 && (e.body?.reason === 'api_model_changed' || e.body?.error === 'api_model_changed')) {
            const checkpointModel = typeof e.body.checkpoint_model === 'string' ? e.body.checkpoint_model : ''
            const currentModel = typeof e.body.current_model === 'string' ? e.body.current_model : ''
            set({ apiModelConflict: { botId, checkpointModel, currentModel } })
            return
          }
          if (isRunStateUncommitted(e, 'start')) {
            get().notify('error', runStateUncommittedText(e))
          } else if (!isActiveRunConflict(e)) {
            throw e
          }
        }
        await get().refreshState()
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
    let stopped = false
    await guarded(set, get, `abort:${botId}`, async () => {
      const r = await api.abortBot(botId)
      const n = r.aborted.length
      stopped = r.keys_sent
      get().notify(
        r.keys_sent ? 'info' : 'error',
        r.keys_sent
          ? t('已中止 {n} 個回合', { n })
          : t('已中止 {n} 個回合，但 esc 送不進終端——agent 那邊可能還在跑，必要時停掉 Bot', { n }),
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
      get().notify('error', t('送不出登入指令：{reason}', { reason: loginErrText(e) }))
      return false
    } finally {
      set((s) => {
        const busy = { ...s.busy }
        delete busy[key]
        return { busy }
      })
    }
  },

  async cancelStartingSend(botId) {
    const pending = startingSend(get().turns[botId], get().messages[botId])
    if (!pending) return
    await guarded(set, get, `withdraw:${botId}`, async () => {
      let withdrawn: Awaited<ReturnType<typeof api.withdrawTurn>>
      try {
        withdrawn = await api.withdrawTurn(pending.turnId)
      } catch (e) {
        get().notify('error', t('撤不回來（可能已經送出）：{reason}', { reason: errText(e) }))
        await get().loadMessages(botId)
        return
      }
      const key = `bot:${botId}` as const
      const text = withdrawn.text || pending.text
      markWithdrawnTurn(set, get, botId, pending.turnId)
      get().setDraft(key, prependDraft(text, get().drafts[key] ?? ''))
      get().setDraftCursor(key, text.length)
      const attachmentCount = withdrawn.attachments.length || pending.attachments
      if (attachmentCount > 0) get().notify('error', t('訊息已放回輸入框，但 {attachmentCount} 個附件要重新加', { attachmentCount }))
      await get().loadMessages(botId)
    })
  },

  async withdrawQueuedSend(botId) {
    const pending = queuedSendFor(get(), botId)
    if (!pending) return
    await guarded(set, get, `withdraw:${botId}`, async () => {
      let withdrawn: Awaited<ReturnType<typeof api.withdrawTurn>>
      try {
        withdrawn = await api.withdrawTurn(pending.turnId)
      } catch (e) {
        get().notify('error', t('撤不回來（可能已經送出）：{reason}', { reason: errText(e) }))
        await Promise.all([get().refreshState(), get().loadMessages(botId)])
        return
      }
      markWithdrawnTurn(set, get, botId, pending.turnId)
      const key = `bot:${botId}` as const
      const text = withdrawn.text || pending.text
      get().setDraft(key, prependDraft(text, get().drafts[key] ?? ''))
      get().setDraftCursor(key, text.length)
      await get().loadMessages(botId)
    })
  },

  async sendPrompt(botId, text, attachments = [], sendNow = false, startIfStopped = false, draft, queueIfBusy = false, cacheTtl) {
    if (isApiKind(get().bots.find((b) => b.id === botId)?.kind)) {
      const r = get().runs[botId]
      if (startIfStopped && !(r && (r.state === 'running' || r.state === 'starting'))) {
        try {
          await api.startBot(botId)
          await get().refreshState()
        } catch (e) {
          if (e instanceof ApiError && e.status === 409 && (e.body?.reason === 'api_model_changed' || e.body?.error === 'api_model_changed')) {
            const checkpointModel = typeof e.body.checkpoint_model === 'string' ? e.body.checkpoint_model : ''
            const currentModel = typeof e.body.current_model === 'string' ? e.body.current_model : ''
            set({ apiModelConflict: { botId, checkpointModel, currentModel } })
            return false
          }
          if (!isActiveRunConflict(e)) {
            get().notify('error', t('啟動失敗：{reason}', { reason: startErrText(e, get().bots.find((b) => b.id === botId)?.kind) }))
            return false
          }
          await get().refreshState()
        }
      }
      sendNow = false
      startIfStopped = false
      queueIfBusy = false
      draft = undefined
    }
    const draftKey = draft ? `:${draft.action}:${draft.token}` : ''
    const reqKey = `send:${botId}:${sendNow ? 1 : 0}:${startIfStopped ? 1 : 0}:${queueIfBusy ? 1 : 0}:${cacheTtl ?? ''}:${text}\u0000${attachments.join(',')}${draftKey}`
    const held = peekRequestId(reqKey)
    if (held && sendIdIsSpent(get().turns[botId], held)) settleCreateRequest(reqKey)
    const crid = createRequestId(reqKey, { maxAgeMs: SEND_RETRY_WINDOW_MS })
    if (draft) set((s) => markDraftBusy(s, botId, draft.action))
    try {
      const res = await api.sendPrompt(botId, text, crid, attachments, sendNow, startIfStopped, draft, queueIfBusy, cacheTtl)
      settleCreateRequest(reqKey)
      set((s) => ({ composerDrafts: withoutKey(s.composerDrafts, botId) }))
      const fell = sendNow ? sendNowFellThrough(res.send_now) : null
      if (fell) {
        get().notify('error', fell.text)
        void get().loadMessages(botId)
        return fell.consumed
      }
      if (sendNow) {
        const note = sendNowNotice(res.send_now, get().bots.find((b) => b.id === botId)?.kind)
        if (note) get().notify(note.level, note.text)
      }
      if (res.delivery === 'unknown') {
        get().notify('info', UNKNOWN_SENT_NOTICE)
      }
      if (res.delivery === 'failed') {
        get().notify('error', t('訊息未送達（delivery=failed），請確認 agent 狀態後重試。'))
        void get().loadMessages(botId)
        return false
      }
      if (res.delivery === 'queued') {
        set((s) => noteQueuedTurn(s, botId, res.turn_id, crid, startIfStopped, queueIfBusy && !startIfStopped))
        if (queueIfBusy && !startIfStopped) {
          await get().loadMessages(botId)
        }
        return true
      }
      const delivery = res.delivery
      set((s) => noteInFlightTurn(s, botId, res.turn_id, crid, get().runs[botId]?.id ?? null, delivery))
      return true
    } catch (e) {
      if (e instanceof ApiError) settleCreateRequest(reqKey)
      const un = asUncommittedSend(e)
      if (un) {
        if (draft) {
          set((s) =>
            un.sent === false
              ? markDraftBusy(s, botId, undefined)
              : { composerDrafts: withoutKey(s.composerDrafts, botId) },
          )
        }
        get().notify('error', uncommittedSendText(un))
        if (un.sent === false) {
          void get().loadMessages(botId)
          return false
        }
        if (un.delivery) {
          const seen: TurnDelivery = un.delivery === 'unknown' ? 'unknown' : 'pending'
          set((s) => noteInFlightTurn(s, botId, un.turnId, crid, get().runs[botId]?.id ?? null, seen))
        }
        return true
      }
      const block = draftBlockFrom(e, get().runs[botId]?.id)
      if (block) {
        set((s) => ({ composerDrafts: { ...s.composerDrafts, [botId]: block } }))
        if (draft) get().notify(e instanceof ApiError && e.body.reason === 'draft_changed' ? 'info' : 'error', errText(e))
        return false
      }
      if (draft) set((s) => (draftIsGone(e) ? { composerDrafts: withoutKey(s.composerDrafts, botId) } : markDraftBusy(s, botId, undefined)))
      if (e instanceof ApiError && e.status === 409 && e.body.reason === 'api_model_changed') {
        set({ apiModelConflict: { botId, checkpointModel: str(e.body.checkpoint_model), currentModel: str(e.body.current_model) } })
        return false
      }
      if (e instanceof ApiError && e.status === 409 && e.body.reason === 'needs_login') {
        get().notify('error', typeof e.body.message === 'string' ? e.body.message : t('這個 claude 還沒登入，先到「終端」分頁完成登入。'))
        return false
      }
      if (queueIfBusy && e instanceof ApiError && e.status === 409 && e.body.reason === 'queue_slot_taken') {
        await Promise.all([get().refreshState(), get().loadMessages(botId)])
        const holder = slotHolderFrom(e.body)
        const queuedNow = queuedSendFor(get(), botId)
        if (holder?.kind === 'user' && attachments.length === 0 && queuedNow && queuedNow.text.trim() === text.trim()) {
          get().notify('info', t('這一句已經在排隊中（可能是另一個分頁剛送出的），沒有再送一次'))
          return true
        }
        const key = `bot:${botId}` as const
        get().setDraft(key, draftWithText(text, get().drafts[key] ?? ''))
        get().setDraftCursor(key, text.length)
        get().notify('error', queueSlotNotice(holder, attachments.length))
        return false
      }
      if (e instanceof ApiError && e.status === 409 && typeof e.body.send_now_message === 'string') {
        get().notify('error', e.body.send_now_message)
        return false
      }
      get().notify('error', startErrText(e, get().bots.find((b) => b.id === botId)?.kind))
      return false
    }
  },

  dismissComposerDraft(botId) {
    bumpDraftEpoch(botId)
    set((s) => ({ composerDrafts: withoutKey(s.composerDrafts, botId) }))
  },

  async loadComposerDraft(botId, opts = {}) {
    const tries = Math.max(1, opts.tries ?? 1)
    const epoch = draftEpoch(botId)
    for (let i = 0; i < tries; i++) {
      if (i > 0) await new Promise((r) => setTimeout(r, opts.delayMs ?? 500))
      try {
        const snap = await api.fetchComposerDraft(botId)
        if (draftEpoch(botId) !== epoch) return null
        const runId = get().runs[botId]?.id
        if (snap.runId && runId && snap.runId !== runId) return null
        if (snap.busy && snap.state !== 'dialog') continue
        if (snap.state === 'empty') {
          if (get().composerDrafts[botId]) set((s) => ({ composerDrafts: withoutKey(s.composerDrafts, botId) }))
          return null
        }
        const block = draftBlockFromSnapshot({ ...snap, runId: snap.runId || runId }, opts.editNow)
        if (block) {
          set((s) => {
            const cur = s.composerDrafts[botId]
            return { composerDrafts: { ...s.composerDrafts, [botId]: cur && cur.token === block.token ? { ...block, editing: cur.editing, editNow: cur.editNow ?? block.editNow } : block } }
          })
          return block
        }
      } catch {
        return null
      }
    }
    return null
  },

  setComposerEditing(botId, flags) {
    set((s) => {
      const cur = s.composerDrafts[botId]
      if (!cur) return {}
      return { composerDrafts: { ...s.composerDrafts, [botId]: { ...cur, ...flags } } }
    })
  },

  dismissLoginPrompt(host, identity, episode) {
    set((s) => ({ loginPromptDismissed: { ...s.loginPromptDismissed, [dismissKey(host, identity)]: episode } }))
  },

  pruneLoginPromptDismissed(pending) {
    const next = pruneDismissed(get().loginPromptDismissed, pending)
    if (next !== get().loginPromptDismissed) set({ loginPromptDismissed: next })
  },

  async acceptSuggestion(botId) {
    const run = get().runs[botId]
    const suggestion = run?.prompt_suggestion
    if (!run || !suggestion) return false
    const reqKey = `suggest:${botId}:${run.id}:${suggestion}`
    const crid = createRequestId(reqKey, { maxAgeMs: SEND_RETRY_WINDOW_MS })
    let sent = false
    await guarded(
      set,
      get,
      `suggest:${botId}`,
      async () => {
        try {
          const res = await api.acceptSuggestion(botId, suggestion, run.id, crid)
          settleCreateRequest(reqKey)
          if (res.delivery === 'unknown') get().notify('info', UNKNOWN_SENT_NOTICE)
          if (res.delivery === 'failed') {
            get().notify('error', t('訊息未送達（delivery=failed），請確認 agent 狀態後重試。'))
            void get().loadMessages(botId)
            return
          }
          if (res.delivery !== 'queued') set((s) => noteInFlightTurn(s, botId, res.turn_id, crid, run.id, res.delivery as TurnDelivery))
          sent = true
        } catch (e) {
          if (e instanceof ApiError) settleCreateRequest(reqKey)
          const block = draftBlockFrom(e, get().runs[botId]?.id)
          if (block) set((s) => ({ composerDrafts: { ...s.composerDrafts, [botId]: block } }))
          const un = asUncommittedSend(e)
          if (un) {
            get().notify('error', uncommittedSendText(un))
            if (un.sent !== false && un.delivery) {
              set((s) => noteInFlightTurn(s, botId, un.turnId, crid, run.id, un.delivery === 'unknown' ? 'unknown' : 'pending'))
              sent = true
            }
            return
          }
          get().notify('error', suggestionFailureText(e, errText(e)))
          void get().refreshState()
        }
      },
    )
    return sent
  },

  async sendKeys(botId, keys, pinRunId) {
    try {
      if (pinRunId) await api.sendKeys(botId, keys, pinRunId)
      else await sendWithFreshRun(get, botId, (runId) => api.sendKeys(botId, keys, runId))
    } catch (e) {
      get().notify('error', t('送出按鍵失敗：{reason}', { reason: errText(e) }))
      throw e
    }
  },

  async sendText(botId, text, enter, record = false, pinRunId) {
    try {
      if (pinRunId) await api.sendText(botId, text, enter, pinRunId, record)
      else await sendWithFreshRun(get, botId, (runId) => api.sendText(botId, text, enter, runId, record))
      return true
    } catch (e) {
      get().notify('error', t('送出文字失敗：{reason}', { reason: errText(e) }))
      return false
    }
  },

  async abandonTurn(botId, turnId) {
    await guarded(set, get, `abandon:${turnId}`, async () => {
      await api.abandonTurn(turnId)
      await get().loadMessages(botId)
    })
  },

  async addIdentity(input) {
    try {
      await api.createIdentity(input)
      await get().refreshState()
      get().notify('info', t('已新增身份 {name}', { name: input.name }))
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
      get().notify('info', host && host !== 'local' ? t('已刪除 {host} 的身份 {name}', { host, name }) : t('已刪除身份 {name}', { name }))
    } catch (e) {
      get().notify('error', errText(e))
    }
  },

  async removeShellIdentity(host, identity) {
    try {
      const result = await api.deleteShellIdentity(host, identity)
      await get().refreshTools(host)
      return { ok: true, backup_path: result.backup_path }
    } catch (e) {
      const body = e instanceof ApiError ? e.body : null
      const refs = Array.isArray(body?.refs)
        ? body.refs.flatMap((ref) =>
            isRec(ref) && typeof ref.type === 'string'
              ? [{ type: ref.type, id: typeof ref.id === 'string' ? ref.id : undefined }]
              : [],
          )
        : undefined
      return {
        ok: false,
        reason: typeof body?.reason === 'string' ? body.reason : undefined,
        message: typeof body?.message === 'string' ? body.message : e instanceof Error ? e.message : String(e),
        backup_path: typeof body?.backup_path === 'string' ? body.backup_path : undefined,
        refs: refs?.length ? refs : undefined,
      }
    }
  },

  async addHost(input) {
    try {
      const res = await api.createHost(input)
      await get().refreshState()
      if (res.connected) get().notify('info', t('主機 {name} 已連線', { name: res.name }))
      else get().notify('error', t('主機 {name} 連線失敗：{reason}', { name: res.name, reason: res.error ?? t('未知原因') }))
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
      get().notify('info', t('已刪除主機 {name}', { name }))
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
      if (r.connected) get().notify('info', t('主機 {name} 已重新連線', { name: r.name }))
      else get().notify('error', t('主機 {name} 重連失敗：{reason}', { name: r.name, reason: r.error ?? t('未知原因') }))
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
      get().notify('info', t('已新增 Project {label}', { label: input.label || input.path }))
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
    const reqKey = input.name_auto ? `add:${projectId}:${input.kind}:${input.identity ?? ''}` : `add:${projectId}:${input.name}`
    try {
      const { id, name, remapped_model: remapped } = await api.createBot(projectId, { ...input, client_request_id: createRequestId(reqKey) })
      settleCreateRequest(reqKey)
      await get().refreshState()
      if (id) get().selectBot(id)
      get().notify('info', remapped ? t('已新增 Bot {name}：「{from}」已停用，改用「{to}」', { name, from: remapped.from, to: remapped.to }) : t('已新增 Bot {name}', { name }))
      return id || null
    } catch (e) {
      if (String(e).includes('request_id_reused')) settleCreateRequest(reqKey)
      const apiReason = e instanceof ApiError ? apiBotStartReasonText(e.body.reason, input.kind) : null
      get().notify('error', apiReason ?? errText(e))
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
    const stem = bot.name.replace(/-\d+$/, '') || bot.name
    let n = 2
    while (taken.has(`${stem}-${n}`)) n += 1
    const name = `${stem}-${n}`
    const key = `clone:${botId}`
    if (s.busy[key]) return null
    const tempId = `pending:${Date.now().toString(36)}`
    const crid = createRequestId(key)
    const placeholder: Bot = { ...bot, id: tempId, name, parent_bot_id: null, pending: true, create_request_id: crid }
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
        name_auto: true,
        client_request_id: crid,
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
      const c = created as unknown as string | { id: string }
      const id = typeof c === 'string' ? c : c.id
      settleCreateRequest(key)
      if (!id) {
        dropPlaceholder()
        return null
      }
      await get().refreshState()
      if (get().bots.some((b) => b.id === tempId)) dropPlaceholder()
      const rest = botsOfProject(get(), bot.project_id).map((b) => b.id).filter((x) => x !== id)
      get().moveBot(id, rest[rest.indexOf(botId) + 1] ?? null)
      get().selectBot(id)
      get().notify('info', t('已新增 Bot {name}', { name }))
      void get().startBot(id)
      return id
    } catch (e) {
      dropPlaceholder()
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
      get().selectBot(id)
      if (res.start_error) {
        get().notify('error', t('已從 {name} fork 出 {name2}，但沒有啟動：{reason}', { name: bot.name, name2: res.name, reason: res.start_error }))
      } else {
        get().notify('info', t('已從 {name} fork 出 {name2}，接續它的對話脈絡', { name: bot.name, name2: res.name }))
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
      get().selectBot(id)
      get().notify('info', t('已把子 agent {name} 升級成頂層 bot {name2}，接續同一段對話', { name: bot.name, name2: res.name }))
    })
    return id
  },

  async patchBot(botId, input) {
    if (Object.keys(input).length === 0) return { needsRestart: false, deferred: false, remappedModel: null }
    let out: PatchBotOutcome | null = null
    await guarded(set, get, `patch:${botId}`, async () => {
      const res = await api.patchBot(botId, input)
      out = { needsRestart: res.needs_restart, deferred: res.live_apply?.deferred === true, remappedModel: res.remapped_model, ...(res.message ? { message: res.message } : {}) }
      if (res.remapped_model) {
        get().notify('info', t('「{from}」已停用，這顆 bot 改用「{to}」', { from: res.remapped_model.from, to: res.remapped_model.to }))
      } else if (res.message) {
        get().notify('info', res.message)
      }
      await get().refreshState()
    })
    return out
  },

  async restartBot(botId, resumeNative = true) {
    let ok = false
    await guarded(set, get, `restart:${botId}`, async () => {
      try {
        await api.restartBot(botId, resumeNative)
      } catch (e: unknown) {
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

  async scheduleUpdateRestart(botIds) {
    await guarded(set, get, 'update-restart', async () => {
      set({ updateRestartScheduled: await api.scheduleUpdateRestart(botIds) })
      get().notify('info', t('已排程：{n} 顆閒下來會自動重啟套用更新', { n: botIds.length }))
    })
  },

  async cancelUpdateRestart(botIds) {
    await guarded(set, get, 'update-restart', async () => {
      set({ updateRestartScheduled: await api.cancelUpdateRestart(botIds) })
    })
  },

  async restartIdleBots() {
    await guarded(set, get, 'restart-idle', async () => {
      const plan = await api.restartIdleBots()
      if (plan.already_running) {
        set({ restartBatch: joinRunningBatch(get().restartBatch, plan.batch_id) })
        get().notify(
          'info',
          plan.restart_status === 'deferred'
            ? t('正在跑的那一批只動某台的 CLI，其餘等著套用的會在它跑完後自動接著重啟')
            : t('已經有一批重啟在跑，這次不另開，進度照那一批顯示'),
        )
        return
      }
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
        get().notify('info', plan.skipped.length > 0 ? t('沒有閒置的 Bot 可以重啟（都在忙）') : t('沒有等著套用更新的 Bot'))
      }
    })
  },

  async startCliUpdate(host, kind, targetVersion) {
    await guarded(set, get, `cli-update:${host}`, async () => {
      set((s) => ({ cliUpdates: [...s.cliUpdates.filter((item) => !(item.host === host && item.kind === kind)), { id: '', host, kind, phase: 'starting', from: null, to: null }] }))
      try {
        const r = await api.startCliUpdate(host, kind, targetVersion)
        set((s) => ({
          cliUpdates: s.cliUpdates.map((item) => (item.host === host && item.kind === kind && item.phase === 'starting' ? { ...item, id: r.update_id } : item)),
        }))
      } catch (e) {
        set((s) => ({ cliUpdates: s.cliUpdates.filter((item) => !(item.host === host && item.kind === kind && item.phase === 'starting')) }))
        throw e
      }
    })
  },

  async installCodexUpdate(host, targetVersion) {
    await get().startCliUpdate(host, 'codex', targetVersion)
  },

  async installClaudeUpdates(hosts, targetVersion) {
    await Promise.allSettled(hosts.map((host) => get().startCliUpdate(host, 'claude', targetVersion)))
  },

  cancelAgmDelete() {
    set({ agmDeleteAsk: null })
  },

  async restoreBot(botId) {
    try {
      await api.restoreBot(botId)
      await get().refreshState()
      return true
    } catch (e) {
      get().notify('error', t('復原失敗：{reason}', { reason: errText(e) }))
      return false
    }
  },

  async removeBot(botId, opts = {}) {
    await guarded(set, get, `remove:${botId}`, async () => {
      const s0 = get()
      const bot = s0.bots.find((b) => b.id === botId)
      const siblings = bot ? s0.bots.filter((b) => b.project_id === bot.project_id) : []
      const i = siblings.findIndex((b) => b.id === botId)
      const next = (siblings[i + 1] ?? siblings[i - 1] ?? null)?.id ?? null
      const name = bot?.name ?? 'Bot'
      try {
        await api.deleteBot(botId, opts)
        get().notify('info', t('已刪除 {name}', { name }), {
          label: t('復原'),
          run: async () => {
            if (await get().restoreBot(botId)) get().selectBot(botId)
          },
        })
        draftSync.forget(`bot:${botId}`)
        set((s) => {
          const drafts = withoutKey(s.drafts, `bot:${botId}`)
          const draftCursors = withoutKey(s.draftCursors, `bot:${botId}`)
          writeDraftCursors(s.draftCursors, draftCursors)
          return {
            selectedBotId: s.selectedBotId === botId ? next : s.selectedBotId,
            settingsBotId: s.settingsBotId === botId ? null : s.settingsBotId,
            drafts,
            draftCursors,
          }
        })
        await get().refreshState()
        if (next === null && get().selectedBotId !== null && !get().bots.some((b) => b.id === botId)) {
          set({ selectedBotId: null })
        }
      } catch (e) {
        const ask = opts.confirmSupervisor ? null : supervisorOwnedAsk(e, botId, name)
        if (ask) set({ agmDeleteAsk: ask })
        else get().notify('error', errText(e))
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
      for (const k of botIds.map((id) => `bot:${id}`)) draftSync.forget(k)
      set((s) => {
        let drafts = s.drafts
        let draftCursors = s.draftCursors
        for (const id of botIds) drafts = withoutKey(drafts, `bot:${id}`)
        for (const id of botIds) draftCursors = withoutKey(draftCursors, `bot:${id}`)
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
    const epoch = ++quotaEpoch
    try {
      const snap = await api.fetchQuota()
      if (epoch !== quotaEpoch) return
      set((s) => {
        const quota = { ...snap }
        for (const [key, at] of quotaDirty) {
          if (at >= epoch && Object.prototype.hasOwnProperty.call(s.quota, key)) quota[key] = s.quota[key]
        }
        return { quota }
      })
    } catch {
    }
  },

  async loadUserTimeZone() {
    try {
      const r = await getTimeZone()
      if (r.time_zone) set({ userTimeZone: r.time_zone })
    } catch {
    }
  },

  setUserTimeZone(zone: string) {
    set({ userTimeZone: zone })
  },

  async loadMem() {
    try {
      set({ mem: await api.fetchMem() })
    } catch {
    }
  },

  async loadModels(kind, host, identity) {
    const key = modelsKey(kind, host, identity)
    const cached = get().models[key]
    if (!shouldFetchModels(cached, get().modelsFailedAt[key], Date.now())) return cached ?? null
    const gen = (modelsReq.get(key) ?? 0) + 1
    modelsReq.set(key, gen)
    const hostName = host || 'local'
    const hostEpoch = hostModelsEpoch.get(hostName) ?? 0
    const stillCurrent = () => modelsReq.get(key) === gen && (hostModelsEpoch.get(hostName) ?? 0) === hostEpoch
    try {
      const res = await api.fetchModels(kind, host, identity)
      if (!stillCurrent()) return get().models[key] ?? null
      const list = res.models
      saveLastModels(key, list)
      set((s) => {
        const { [key]: _gone, ...failed } = s.modelsFailedAt
        return {
          models: { ...s.models, [key]: list },
          modelsMeta: {
            ...s.modelsMeta,
            [key]: { source: res.source, source_host: res.source_host, stale: res.stale },
          },
          modelsFailedAt: failed,
        }
      })
      return list
    } catch {
      if (!stillCurrent()) return get().models[key] ?? null
      set((s) => ({
        models: { ...s.models, [key]: null },
        modelsMeta: { ...s.modelsMeta, [key]: { source: 'static', stale: true } },
        modelsFailedAt: { ...s.modelsFailedAt, [key]: Date.now() },
      }))
      return null
    }
  },

  dropHostModels(host) {
    const hostName = host || 'local'
    hostModelsEpoch.set(hostName, (hostModelsEpoch.get(hostName) ?? 0) + 1)
    set((s) => ({
      models: dropHostModels(s.models, host),
      modelsMeta: dropHostModels(s.modelsMeta, host),
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
      get().dropHostModels(target)
      return true
    } catch (e) {
      get().notify('error', t('重新偵測 {host} 失敗：{reason}', { host: host || t('本機'), reason: errText(e) }))
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
      get().notify('info', t('已請 {bot} 安裝並登入 {kind}', { bot: get().bots.find((b) => b.id === viaBotId)?.name ?? viaBotId, kind }))
      return res.turn_id
    } catch (e) {
      get().notify('error', t('安裝 {kind} 失敗：{reason}', { kind, reason: errText(e) }))
      return null
    } finally {
      set((s) => ({ busy: { ...s.busy, [key]: false } }))
    }
  },

  setKindDisplay: (mode) => {
    try {
      localStorage.setItem(KIND_DISPLAY_KEY, mode)
    } catch {
    }
    set({ kindDisplay: mode })
  },

  dismissToolHint: () => set({ toolHintDismissed: true }),

  setDraft: (key, text) => {
    const before = get().drafts[key] ?? ''
    set((s) => {
      if ((s.drafts[key] ?? '') === text) {
        if (text || !s.draftCursors[key]) return {}
        const draftCursors = withoutKey(s.draftCursors, key)
        writeDraftCursors(s.draftCursors, draftCursors)
        return { draftCursors }
      }
      const drafts = text ? { ...s.drafts, [key]: text } : withoutKey(s.drafts, key)
      const draftCursors = text ? s.draftCursors : withoutKey(s.draftCursors, key)
      if (!text) writeDraftCursors(s.draftCursors, draftCursors)
      return { drafts, draftCursors }
    })
    if (before !== text) draftSync.localChange(key, text)
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

  readTerminal: (botId, source, lines, format) => api.fetchTerminal(botId, source, lines, format),

  async openHostShell(host, cwd) {
    let ok = false
    await guarded(set, get, `shell:${host}`, async () => {
      const existing = cwd ? [] : await api.fetchHostShells(host)
      const reuse = existing.length > 0 ? existing[existing.length - 1] : null
      const shell = reuse ?? (await api.openHostShell(host, cwd))
      set({ shellView: { host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null })
      ok = true
    })
    return ok
  },

  async loginIdentity(host, identity, opts) {
    const kind = (identityStatusOfHost(get(), host)[identity]?.kind ?? 'claude') as BotKind
    if (!(await get().confirmCodexLoginReplace(host, kind, identity))) return false
    return identityAuth(set, get, host, identity, 'login', opts?.background === true)
  },

  openIdentityLoginPane() {
    const pane = get().identityLoginPane
    if (!pane) return
    markUserNav()
    set({ shellView: { host: pane.host, paneId: pane.paneId, cwd: pane.cwd }, settingsBotId: null })
  },

  confirmCodexLoginReplace(host, kind, identity) {
    const s = get()
    const loggedIn = identity
      ? identityStatusOfHost(s, host)[identity]?.logged_in
      : (host && host !== 'local' ? s.hosts.find((h) => h.name === host)?.tools : s.localTools)?.[kind]?.logged_in
    if (!codexLoginReplacesLogin(kind, loggedIn)) return Promise.resolve(true)
    get().codexLoginAsk?.resolve(false)
    return new Promise<boolean>((resolve) => set({ codexLoginAsk: { resolve } }))
  },

  answerCodexLoginAsk(ok) {
    const ask = get().codexLoginAsk
    set({ codexLoginAsk: null })
    ask?.resolve(ok)
  },

  setApiModelConflict(conflict) {
    set({ apiModelConflict: conflict })
  },

  async resolveApiModelConflict(action) {
    const conflict = get().apiModelConflict
    if (!conflict) return
    set({ apiModelConflict: null })
    const { botId, checkpointModel } = conflict
    if (action === 'revert') {
      try {
        await api.patchBot(botId, { model: checkpointModel })
        get().notify('info', t('已換回原模型 {model}', { model: checkpointModel }))
        await get().startBot(botId)
      } catch (e) {
        get().notify('error', t('換回模型失敗：{reason}', { reason: errText(e) }))
      }
    } else if (action === 'clear') {
      try {
        await retireApiSession(botId)
        get().notify('info', t('已清除對話，下一則從新模型開始'))
        await get().startBot(botId)
      } catch (e) {
        get().notify('error', t('清除對話失敗：{reason}', { reason: errText(e) }))
      }
    }
    await get().refreshState()
  },

  async logoutIdentity(host, identity) {
    return identityAuth(set, get, host, identity, 'logout')
  },

  async logoutKindDefault(host, kind) {
    if (kind === 'agy') return get().logoutAgy(host)
    let ok = false
    await guarded(set, get, `kind-logout:${host || 'local'}:${kind}`, async () => {
      const shell = await api.logoutKindDefault(host, kind)
      if (shell) set({ shellView: { host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null })
      ok = true
    })
    return ok
  },

  async logoutAgy(host) {
    let ok = false
    await guarded(set, get, `agy-logout:${host || 'local'}`, async () => {
      const { removed } = await api.logoutAgy(host)
      const keys = [quotaKey(host, 'agy')]
      const loggedOut = (t: ToolMap): ToolMap => ({ ...t, agy: { ...t.agy, logged_in: false } })
      set((s) => ({
        quota: { ...s.quota, ...Object.fromEntries(keys.map((k) => [k, null])) },
        ...(!host || host === 'local'
          ? { localTools: loggedOut(s.localTools) }
          : { hosts: s.hosts.map((h) => (h.name === host ? { ...h, tools: loggedOut(h.tools) } : h)) }),
      }))
      get().notify('info', removed ? t('agy 已登出，憑證已清除') : t('agy 本來就沒有憑證（已是未登入）'))
      ok = true
    })
    return ok
  },

  async installAgy(host) {
    let ok = false
    await guarded(
      set,
      get,
      `agy-install:${host || 'local'}`,
      async () => {
        const where = !host || host === 'local' ? t('本機') : host
        get().notify('info', t('開始在 {where} 安裝 agy（下載約 50 MB 並驗 sha512）…', { where }))
        const r = await api.installAgy(host)
        get().notify('info', r.already_latest ? t('{where} 的 agy 已是官方最新版 {to}', { where, to: r.to }) : t('{where} 已安裝 agy {to}{path}；要登入請到額度欄開 shell 登入', { where, to: r.to, path: r.path ? t('（{path}）', { path: r.path }) : '' }))
        ok = true
      },
      (e) => t('安裝 agy 失敗：{reason}', { reason: errText(e) }),
    )
    return ok
  },

  async loadIdentityPrefs() {
    const seq = ++identityPrefsLoadSeq
    for (;;) {
      const epoch = identityPrefsEpoch
      try {
        const list = await api.fetchDisabledIdentities()
        if (seq !== identityPrefsLoadSeq) return
        if (epoch !== identityPrefsEpoch) continue
        set({ disabledIdentities: list })
        api.fetchDisabledKinds().then((kinds) => set({ disabledKinds: kinds })).catch(() => {})
      } catch {
        if (seq === identityPrefsLoadSeq) identityPrefsAsked = false
      }
      return
    }
  },

  async setKindDisabled(host, kind, disabled) {
    const key = api.kindPrefKey(host, kind)
    await guarded(set, get, `kind-disabled:${key}`, async () => {
      await api.setKindDisabled(host, kind, disabled)
      set((s) => ({
        disabledKinds: disabled ? (s.disabledKinds.includes(key) ? s.disabledKinds : [...s.disabledKinds, key]) : s.disabledKinds.filter((k) => k !== key),
      }))
    })
  },

  async setIdentityDisabled(host, kind, name, disabled) {
    const key = api.identityPrefKey(host, kind, name)
    await guarded(set, get, `identity-disabled:${key}`, async () => {
      await api.setIdentityDisabled(host, kind, name, disabled)
      identityPrefsEpoch += 1
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

  refreshPanes: singleFlight(async () => {
    let all: ProjectPane[]
    try {
      all = await api.fetchAllPanes()
    } catch {
      return
    }
    const unowned = await api.fetchUnownedPanes().catch(() => all.filter((p) => !p.project_id))
    set({ sidePanes: groupByProject(all), unownedPanes: unowned })
  }),

  async closeTracedPane(pane, confirm) {
    try {
      await api.closePane(pane.pane_id, pane.host, confirm)
    } catch (e) {
      const fresh = api.servicePaneConflict(e)
      if (fresh) {
        set((s) => withPane(s, fresh))
        return fresh
      }
      if (e instanceof ApiError && e.status === 404) {
        get().paneGone(pane.host, pane.pane_id)
        return 'closed'
      }
      get().notify('error', t('關閉失敗：{reason}', { reason: errText(e) }))
      return null
    }
    dropPane(set, pane.host, pane.pane_id)
    void get().refreshPanes()
    return 'closed'
  },

  paneGone(host, paneId) {
    dropPane(set, host, paneId)
    get().notify('info', t('這顆 pane 已經關掉了'))
    void get().refreshPanes()
  },

  viewPane: (pane) => {
    markUserNav()
    set({
      shellView: { host: pane.host, paneId: pane.pane_id, cwd: pane.cwd ?? '', readOnly: paneReadOnly(pane), traced: true },
      settingsBotId: null,
    })
  },

  viewHostShell: (shell) => {
    markUserNav()
    set({ shellView: { host: shell.host, paneId: shell.pane_id, cwd: shell.cwd }, settingsBotId: null })
  },

  closeShellView: () => {
    markUserNav()
    set({ shellView: null })
  },

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
    const still = (next: ShellView | null) =>
      set((s) => (s.shellView && s.shellView.host === v.host && s.shellView.paneId === v.paneId ? { shellView: next } : {}))
    try {
      const alive = await api.fetchHostShells(v.host)
      if (alive.some((sh) => sh.pane_id === v.paneId)) return still({ ...v, readOnly: false, traced: false })
      const tracked = (await api.fetchAllPanes()).find((p) => p.host === v.host && p.pane_id === v.paneId)
      still(tracked ? { ...v, readOnly: paneReadOnly(tracked), traced: true } : null)
    } catch {
    }
  },

  async endHostShell(host, paneId, confirm = false) {
    let needs: api.CloseNeedsConfirm | null = null
    await guarded(set, get, `shell:${host}:${paneId}`, async () => {
      try {
        const own = await api.fetchHostShells(host)
        if (own.some((sh) => sh.pane_id === paneId)) {
          await api.closeHostShell(host, paneId, confirm)
        } else {
          try {
            await api.closePane(paneId, host, confirm)
          } catch (e) {
            if (!(e instanceof ApiError && e.status === 404)) throw e
          }
        }
      } catch (e) {
        needs = api.closeNeedsConfirm(e)
        if (needs) return
        throw e
      }
      set((s) =>
        s.shellView && s.shellView.host === host && s.shellView.paneId === paneId ? { shellView: null } : {},
      )
    })
    return needs
  },

  ...(initialStoreData as Partial<StoreState>),
}
})

interface LegacyQueuedSend {
  text: string
  attachments: string[]
  turnId?: string
}

async function migrateLegacyQueuedSends(get: GetFn): Promise<void> {
  if (legacyQueueMigrationAttempted) return
  legacyQueueMigrationAttempted = true
  const state = get() as StoreState & { queuedSends?: Record<string, LegacyQueuedSend> }
  const legacyRows = state.queuedSends
  if (!legacyRows) return
  const pending = Object.entries(legacyRows).filter(([botId, row]) => row && !row.turnId && !queuedSendFor(state, botId))
  const cleanState = { ...state } as StoreState & { queuedSends?: Record<string, LegacyQueuedSend> }
  delete cleanState.queuedSends
  useStore.setState(cleanState, true)
  for (const [botId, row] of pending) {
    if (!row.text && row.attachments.length === 0) continue
    const accepted = await get().sendPrompt(botId, row.text, row.attachments, false, false, undefined, true)
    if (!accepted) {
      const key = `bot:${botId}` as const
      const current = get().drafts[key] ?? ''
      const restored = draftWithText(row.text, current)
      if (restored !== current) get().setDraft(key, restored)
      if (row.attachments.length > 0) {
        get().notify('error', t('舊分頁的訊息已退回輸入框，但 {count} 個附件要重新加', { count: row.attachments.length }))
      }
    }
  }
}

function markWithdrawnTurn(set: SetFn, get: GetFn, botId: string, turnId: string): void {
  const turn = get().turns[botId]?.[turnId]
  if (!turn || turn.status !== 'queued') return
  set((state) => ({
    turns: {
      ...state.turns,
      [botId]: {
        ...(state.turns[botId] ?? {}),
        [turnId]: { ...turn, status: 'failed', delivery: 'failed', completed_at: new Date().toISOString() },
      },
    },
  }))
}

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

function reportStateRefreshError(set: SetFn, get: GetFn, e: unknown) {
  const text = t('同步狀態失敗：{reason}', { reason: errText(e) })
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

function existingLoginPane(e: unknown): string | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'identity_login_in_progress') return null
  const id = e.body.pane_id
  return typeof id === 'string' && id ? id : null
}

async function identityAuth(set: SetFn, get: GetFn, host: string, identity: string, op: 'login' | 'logout', background = false) {
  let ok = false
  await guarded(set, get, `identity-${op}:${host}:${identity}`, async () => {
    const show = (paneId: string, cwd: string) => {
      if (background) {
        set({ identityLoginPane: { host, identity, paneId, cwd } })
        return
      }
      markUserNav()
      set({ shellView: { host, paneId, cwd }, settingsBotId: null })
    }
    let shell
    try {
      shell = op === 'login' ? await api.loginIdentity(host, identity) : await api.logoutIdentity(host, identity)
    } catch (e) {
      const existing = op === 'login' ? existingLoginPane(e) : null
      if (!existing) throw e
      show(existing, '')
      get().notify('info', t('這個身份已有登入終端，已重新打開；接著完成授權就好'))
      ok = true
      return
    }
    show(shell.pane_id, shell.cwd)
    ok = true
  })
  return ok
}

let disconnect: (() => void) | null = null
let openedOnce = false
let seenSeq = 0
let quotaSweep: ReturnType<typeof setInterval> | null = null
const QUOTA_SWEEP_MS = 5 * 60_000
let quotaEpoch = 0
const quotaDirty = new Map<string, number>()
const modelsReq = new Map<string, number>()
const hostModelsEpoch = new Map<string, number>()

function noteQuotaDirty(keys: string[]) {
  for (const key of keys) if (key) quotaDirty.set(key, quotaEpoch)
}

function turnStillOpen(status: Turn['status']): boolean {
  return status === 'in_flight' || status === 'queued'
}

useStore.subscribe((s, prev) => {
  if (s.runs === prev.runs) return
  const kept = pruneStaleDrafts(s.composerDrafts, s.runs)
  if (kept === s.composerDrafts) return
  for (const botId of Object.keys(s.composerDrafts)) if (!(botId in kept)) bumpDraftEpoch(botId)
  useStore.setState({ composerDrafts: kept })
})

export function resetStoreForTest(): void {
  resetAttachmentDraftsForTest()
  for (const timer of noticeTimers.values()) clearTimeout(timer)
  noticeTimers.clear()
  for (const slot of liveThrottle.values()) clearTimeout(slot.timer)
  liveThrottle.clear()
  knownBotIds = new Set()
  goneBotIds.clear()
  disconnect?.()
  disconnect = null
  if (quotaSweep) clearInterval(quotaSweep)
  quotaSweep = null
  openedOnce = false
  identityPrefsEpoch = 0
  seenSeq = 0
  ephemeralSeq = 0
  appliedStateSeq = 0
  legacyQueueMigrationAttempted = false
  draftSync.dispose()
  ;(useStore.getState().refreshState as { resetForTest?: () => void }).resetForTest?.()
  useStore.setState(useStore.getInitialState(), true)
}

export function dispatchFrameForTest(frame: { seq?: number; type: string; data?: unknown }) {
  if (!boundSet || !boundGet) throw new Error(t('store 還沒建立'))
  handleFrame(boundSet, boundGet, frame)
}

function reloadUpstreamUpdates(set: SetFn, get: GetFn) {
  void loadUpstreamUpdates(() => api.rawTransport.request('GET', '/upstream-updates'), get().notify, (item) => {
    set((s) => ({ upstreamUpdates: { ...s.upstreamUpdates, [item.kind]: item } }))
  })
}

const draftEpochs = new Map<string, number>()
const draftEpoch = (botId: string) => draftEpochs.get(botId) ?? 0
const bumpDraftEpoch = (botId: string) => draftEpochs.set(botId, draftEpoch(botId) + 1)

const draftProbing = new Set<string>()

export function probeComposerDraftOnce(get: GetFn, botId: string): void {
  if (botId.startsWith('pending:') || draftProbing.has(botId)) return
  const s = get()
  const bot = s.bots.find((b) => b.id === botId)
  const run = s.runs[botId]
  const cur = s.composerDrafts[botId]
  if (cur && draftIsStale(cur, run?.id)) get().dismissComposerDraft(botId)
  if (cur && (cur.busy || cur.editing || cur.editNow)) return
  if (!bot || !run || run.state !== 'running') return
  if (!get().composerDrafts[botId] && run.agent_status !== 'idle') return
  if (!['claude', 'codex', 'grok'].includes(bot.kind)) return
  draftProbing.add(botId)
  void get().loadComposerDraft(botId).finally(() => draftProbing.delete(botId))
}

function connectSocket(set: SetFn, get: GetFn) {
  disconnect?.()
  disconnect = api.openSocket({
    since: () => get().lastDurableSeq,
    onStatus: (socket) => {
      if (socket === 'open') {
        resetStateSeq()
        seenSeq = 0
        set({ socket, stateStale: false })
        void get().refreshState().then(() => {
          const selected = get().selectedBotId
          if (selected) probeComposerDraftOnce(get, selected)
        })
        const openedBefore = openedOnce
        if (openedOnce) void reloadLoadedConversations(get)
        openedOnce = true
        flushUnsentReads()
        void get().loadQuota()
        void draftSync.load().catch(() => {})
        if (openedBefore) void get().loadIdentityPrefs()
        if (openedBefore) reloadUpstreamUpdates(set, get)
        if (openedBefore) void get().refreshPanes()
        if (openedBefore) set((s) => bumpEventRevs(s))
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

function botIdsNeedingConversationReload(get: GetFn): string[] {
  const botIds = new Set(loadedBotIdsOf(get))
  const selected = get().selectedBotId
  if (selected && !selected.startsWith('pending:')) botIds.add(selected)
  for (const [botId, turns] of Object.entries(get().turns)) {
    if (Object.values(turns).some((turn) => turnStillOpen(turn.status))) botIds.add(botId)
  }
  return [...botIds]
}

export async function reloadLoadedConversations(get: GetFn): Promise<void> {
  for (const botId of botIdsNeedingConversationReload(get)) await get().loadMessages(botId)
}

const resyncTrigger = (() => {
  let ctx: { set: SetFn; get: GetFn } | null = null
  const runner = createResyncRunner(async () => {
    const { set, get } = ctx!
    resetStateSeq()
    try {
      await get().refreshState()
      set((s) => bumpEventRevs(s))
      await get().loadQuota()
      void get().loadIdentityPrefs()
      reloadUpstreamUpdates(set, get)
      void draftSync.load().catch(() => {})
      void get().refreshPanes()
      for (const botId of botIdsNeedingConversationReload(get)) await get().loadMessages(botId)
      } catch (e) {
      reportStateRefreshError(set, get, e)
    }
  })
  return (set: SetFn, get: GetFn) => {
    ctx = { set, get }
    runner()
  }
})()

function noteTurnDone(set: SetFn, get: GetFn, botId: string, turnId: string) {
  if (viewingBot(get(), botId) && windowActive()) {
    get().markBotRead(botId)
    return
  }
  const s = get()
  if (isExpertBot(s.bots.find((b) => b.id === botId), s.projects) && relayTurnIds(s.messages[botId] ?? []).has(turnId)) return
  if (!takeTurnCompletion(botId, turnId)) return
  set((s) => ({ botUnread: { ...s.botUnread, [botId]: (s.botUnread[botId] ?? 0) + 1 } }))
  persistUnread(get())
}

let ephemeralSeq = 0

export function seqAfterFrame(prev: { lastSeq: number; lastDurableSeq: number }, type: string, seq: number) {
  const durable = type !== 'turn_progress' && type !== 'resync'
  return {
    lastSeq: Math.max(prev.lastSeq, seq),
    lastDurableSeq: durable ? Math.max(prev.lastDurableSeq, seq) : prev.lastDurableSeq,
  }
}

export function handleFrame(set: SetFn, get: GetFn, frame: { seq?: number; type: string; data?: unknown }) {
  const gate = gateFrame(seenSeq, frame.type, frame.seq)
  if (gate.skip) return
  seenSeq = gate.seen
  if (typeof frame.seq === 'number') {
    const seq = frame.seq
    if (frame.type === 'turn_progress') {
      ephemeralSeq = Math.max(ephemeralSeq, seq)
    } else {
      set((s) => {
        const next = seqAfterFrame(s, frame.type, seq)
        return ephemeralSeq > next.lastSeq ? { ...next, lastSeq: ephemeralSeq } : next
      })
    }
  }
  const data = frame.data
  switch (frame.type) {
    case 'resync': {
      resyncTrigger(set, get)
      return
    }
    case 'daemon_status': {
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
    case 'supervisor_changed':
      set((s) => ({ supervisorRev: s.supervisorRev + 1 }))
      return
    case 'panes_changed': {
      void get().refreshPanes()
      return
    }
    case 'draft_updated': {
      if (!isRec(data)) return
      const key = str(pick(data, 'key'))
      const rev = num(pick(data, 'rev'), 0)
      if (!key || key.startsWith('group:') || rev <= 0) return
      draftSync.remote({ key, text: str(pick(data, 'text')), rev, client_id: str(pick(data, 'client_id')) })
      return
    }
    case 'schedules_changed': {
      window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: isRec(data) ? { project_id: str(pick(data, 'project_id')), id: str(pick(data, 'id')) } : {} }))
      return
    }
    case 'schedule_notice': {
      if (!isRec(data)) return
      const occ = str(pick(data, 'occurrence_id'))
      const status = str(pick(data, 'status'))
      const key = `${occ}:${String(pick(data, 'attempt_no') ?? 1)}:${status}`
      if (!occ || seenScheduleNotices.has(key)) return
      seenScheduleNotices.add(key)
      if (seenScheduleNotices.size > 500) seenScheduleNotices.clear()
      const code = str(pick(data, 'schedule_code'))
      const name = `${code ? `${code} ` : ''}${str(pick(data, 'schedule_name')) || t('排程')}`
      const error = str(pick(data, 'error'))
      const outcome = str(pick(data, 'outcome'))
      if (status === 'unknown') useStore.getState().notify('error', t('排程「{name}」上次結果不明，請先到 Bot 對話確認；確認後可重新啟用', { name }))
      else if (outcome && outcome !== 'success') {
        useStore.getState().notify('error', t('排程「{name}」任務結果：{result}', { name, result: outcome === 'refused' ? t('拒絕執行') : outcome === 'failed' ? t('失敗') : t('結果不明，請查看回覆') }))
      } else if (status === 'done') useStore.getState().notify('info', t('排程「{name}」已完成', { name }))
      else if (status === 'failed') useStore.getState().notify('error', error ? t('排程「{name}」失敗：{reason}', { name, reason: error }) : t('排程「{name}」失敗', { name }))
      return
    }
    case 'herdr_recovery_done': {
      if (!isRec(data)) return
      const recoveryId = str(pick(data, 'recovery_id'))
      if (!recoveryId || seenHerdrRecoveryNotices.has(recoveryId)) return
      seenHerdrRecoveryNotices.add(recoveryId)
      if (seenHerdrRecoveryNotices.size > 500) seenHerdrRecoveryNotices.clear()
      const resumed = arr(pick(data, 'resumed'))
      const failed = arr(pick(data, 'failed'))
      const children = arr(pick(data, 'children_lost'))
      const names = (rows: unknown[]) => rows.map((row) => isRec(row) ? str(pick(row, 'name')) : '').filter(Boolean)
      const failedNames = names(failed)
      const childNames = names(children)
      const suffix = bool(pick(data, 'limited'))
        ? t('短時間內重連次數過多，已達自動接回上限。')
        : str(pick(data, 'skipped')) ? t('連線狀態再次改變，部分接回已略過。') : ''
      const detail = [
        failedNames.length ? t('失敗：{names}', { names: failedNames.join(t('、')) }) : '',
        childNames.length ? t('遺失 child：{names}', { names: childNames.join(t('、')) }) : '',
        suffix,
      ].filter(Boolean).join(t('；'))
      const text = t('herdr 意外重啟：接回 {resumed} 顆頂層 bot，{failed} 顆失敗，{children} 個 child pane 遺失。{detail}', { resumed: resumed.length, failed: failed.length, children: children.length, detail })
      get().notify(failed.length || bool(pick(data, 'limited')) ? 'error' : 'info', text.slice(0, 500))
      return
    }
    case 'herdr_integration_auto_install': {
      const n = autoInstallNotice(data)
      if (n) get().notify(n.kind, n.text)
      return
    }
    case 'kind_prefs_changed': {
      if (!isRec(data)) return
      const key = api.kindPrefKey(str(pick(data, 'host')), str(pick(data, 'kind')))
      const on = bool(pick(data, 'disabled'), false)
      set((s) => ({
        disabledKinds: on ? (s.disabledKinds.includes(key) ? s.disabledKinds : [...s.disabledKinds, key]) : s.disabledKinds.filter((k) => k !== key),
      }))
      return
    }
    case 'identity_prefs_changed': {
      if (!isRec(data)) return
      const key = api.identityPrefKey(str(pick(data, 'host')), str(pick(data, 'kind')), str(pick(data, 'identity')))
      const on = bool(pick(data, 'disabled'), false)
      identityPrefsEpoch += 1
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
      if (!name) {
        if (str(pick(data, 'host'))) void get().refreshState()
        return
      }
      const wasConnected = name === 'local'
        ? get().connected
        : get().hosts.find((h) => h.name === name)?.connected ?? false
      if (!wasConnected && bool(pick(data, 'connected'), false)) get().dropHostModels(name)
      if (name === 'local') {
        set({
          connected: bool(pick(data, 'connected'), get().connected),
          ...(pick(data, 'tools') !== undefined ? { localTools: toToolMap(pick(data, 'tools')) } : {}),
          ...(pick(data, 'herdr') !== undefined ? { localHerdr: toHerdrVersion(pick(data, 'herdr')) } : {}),
          ...(pick(data, 'baseline') !== undefined ? { localBaseline: toHostBaseline(pick(data, 'baseline')) } : {}),
          ...(pick(data, 'identities') !== undefined
            ? { localIdentityStatus: toIdentityStatusMap(pick(data, 'identities')) }
            : {}),
          ...(pick(data, 'claude_login_expires_at') !== undefined
            ? { localClaudeLoginExpiresAt: toClaudeLoginExpiresAt(pick(data, 'claude_login_expires_at')) }
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
      const host = str(record ? pick(record, 'host') : undefined) || projectHostName(get(), bot?.project_id ?? null)
      const target = botStatusConnTarget({
        connected: record && record.connected !== undefined ? bool(record.connected, true) : undefined,
        host,
        defaultSession,
      })
      const limitHit = record && 'limit_hit' in record ? toBotLimitHit(record.limit_hit) : undefined
      const limitChanged = limitHit !== undefined && bot !== undefined && JSON.stringify(bot.limit_hit ?? null) !== JSON.stringify(limitHit)
      set((s) => ({
        runs: { ...s.runs, [botId]: run },
        ...(limitChanged ? { bots: s.bots.map((b) => (b.id === botId ? { ...b, limit_hit: limitHit } : b)) } : {}),
        ...(target.kind === 'global' ? { connected: target.connected } : {}),
        ...(target.kind === 'default' ? { defaultConnected: target.connected } : {}),
        ...(target.kind === 'host'
          ? { hosts: mergeHosts(s.hosts, [{ name: target.host, connected: target.connected }]) }
          : {}),
      }))
      if (!wasWorking && run?.agent_status === 'working') clearHookCompletion(botId)
      if (wasWorking && run?.agent_status === 'idle') {
        const map = get().turns[botId] ?? {}
        const turnIds = Object.keys(map)
        const latest = turnIds.length > 0 ? turnIds.reduce((a2, b2) => (a2 > b2 ? a2 : b2)) : null
        const key = idleEdgeCompletionKey(botId, run.id, latest ? map[latest] : null)
        if (key && !(latest && key === latest && isKeepWarmRequestId(map[latest]?.client_request_id))) {
          noteTurnDone(set, get, botId, key)
        }
      }
      return
    }
    case 'messages_rewound': {
      const botId = frameBotId(data)
      const id = isRec(data) ? str(pick(data, 'message_id')) : ''
      if (!botId || !id) return
      const at = (isRec(data) && str(pick(data, 'rewound_at'))) || new Date().toISOString()
      set((s) => {
        const patch: Partial<StoreState> = {}
        const next = markRewound(s.messages[botId] ?? [], id, at)
        if (next) patch.messages = { ...s.messages, [botId]: next }
        return patch
      })
      return
    }
    case 'message_added': {
      const botId = frameBotId(data)
      if (botId && goneBotIds.has(botId)) return
      const msg = toMessage(unwrap(data, 'message'), botId ?? undefined)
      if (!msg || !botId) {
        const sel = get().selectedBotId
        if (sel) void get().loadMessages(sel)
        return
      }
      if (msg.role === 'assistant' && (!msg.turn_id || liveThrottle.get(botId)?.turnId === msg.turn_id)) {
        clearLiveThrottle(botId, msg.turn_id ?? undefined)
      }
      set((s) => {
        const patch: Partial<StoreState> = {}
        const more: Record<string, boolean> = {}
        const sameShown = (a: Message, b: Message) =>
          a.content === b.content && a.source === b.source && a.incomplete === b.incomplete && a.relay_from === b.relay_from && a.created_at === b.created_at
        const grown = upsertSorted(s.messages[botId] ?? [], msg, byTime, sameShown)
        if (grown) {
          const cut = capList(grown, capFor(s.messageCapFloors, botId))
          patch.messages = { ...s.messages, [botId]: cut.list }
          if (cut.trimmed) more[botId] = true
        }
        if (msg.role === 'assistant' && s.liveReply[botId] && (!msg.turn_id || s.liveReply[botId].turnId === msg.turn_id)) {
          patch.liveReply = withoutKey(s.liveReply, botId)
        }
        if (Object.keys(more).length > 0) patch.moreMessages = { ...s.moreMessages, ...more }
        return patch
      })
      const keepWarmTurn = msg.keep_warm === true || (msg.turn_id != null && isKeepWarmRequestId(get().turns[botId]?.[msg.turn_id]?.client_request_id))
      if (completesTurn(msg) && !keepWarmTurn) {
        const turnId = completionKey(msg, Object.keys(get().turns[botId] ?? {}))
        markHookCompletion(botId)
        noteTurnDone(set, get, botId, turnId)
      } else if (msg.role === 'assistant' && keepWarmTurn) {
        markHookCompletion(botId)
      }
      return
    }
    case 'turn_updated': {
      const botId = frameBotId(data)
      const turn = toTurn(unwrap(data, 'turn'), botId ?? undefined)
      if (!turn || !botId || goneBotIds.has(botId)) return
      if (turn.status !== 'in_flight' && turn.status !== 'queued') clearLiveThrottle(botId, turn.id)
      set((s) => ({
        turns: { ...s.turns, [botId]: pruneTurns({ ...(s.turns[botId] ?? {}), [turn.id]: turn }) },
        liveReply:
          turn.status !== 'in_flight' && s.liveReply[botId]?.turnId === turn.id ? withoutKey(s.liveReply, botId) : s.liveReply,
      }))
      if (turn.status !== 'in_flight' && turn.status !== 'queued') {
        markHookCompletion(botId)
        if (!isKeepWarmRequestId(turn.client_request_id)) {
          noteTurnDone(set, get, botId, turn.id)
        }
      }
      return
    }
    case 'turn_progress': {
      const botId = frameBotId(data)
      if (!botId || !isRec(data)) return
      const turnId = str(pick(data, 'turn_id'))
      if (!turnId) return
      const text = str(pick(data, 'text'))
      const activity = str(pick(data, 'activity'))
      const alert = str(pick(data, 'alert'))
      const revision = Number(pick(data, 'revision') ?? 0) || 0
      const estRaw = pick(data, 'est_cost_microusd')
      const estCostMicrousd = typeof estRaw === 'number' && Number.isFinite(estRaw) && estRaw >= 0 ? estRaw : null
      const pendingKey = botId
      const apply = (trailing: boolean) =>
        set((s) => {
          const prev = s.liveReply[botId]
          if (prev && prev.turnId === turnId && prev.revision > revision) return {}
          if (trailing) {
            const st = s.turns[botId]?.[turnId]?.status
            if (st !== undefined && st !== 'in_flight') return {}
          }
          return { liveReply: { ...s.liveReply, [botId]: { turnId, text, activity, alert, revision, estCostMicrousd } } }
        })
      const slot = liveThrottle.get(pendingKey)
      if (slot) {
        slot.turnId = turnId
        slot.apply = () => apply(true)
        return
      }
      apply(false)
      const entry = { turnId, apply: null as null | (() => void), timer: 0 as unknown as ReturnType<typeof setTimeout> }
      entry.timer = setTimeout(() => {
        if (liveThrottle.get(pendingKey) !== entry) return
        liveThrottle.delete(pendingKey)
        entry.apply?.()
      }, LIVE_THROTTLE_MS)
      liveThrottle.set(pendingKey, entry)
      return
    }
    case 'mem_updated': {
      set({ mem: toMemSnapshot(data) })
      return
    }
    case 'upstream_update': {
      const item = applyUpstreamItem(data, get().notify)
      if (item) set((s) => ({ upstreamUpdates: { ...s.upstreamUpdates, [item.kind]: item } }))
      return
    }
    case 'codex_reset_updated': {
      receiveCodexReset(data, get().notify)
      return
    }
    case 'ledger_updated': {
      if (!isRec(data)) return
      const mid = str(pick(data, 'source_message_id'))
      set((s) => ({
        ledgerAnyRev: s.ledgerAnyRev + 1,
        ...(mid ? { ledgerRev: { ...s.ledgerRev, [mid]: (s.ledgerRev[mid] ?? 0) + 1 } } : {}),
      }))
      return
    }
    case 'quota_updated': {
      if (!isRec(data)) return
      const before = get().quota
      const kinds = pick(data, 'kinds')
      if (isRec(kinds)) {
        const entries = Object.entries(kinds).map(([k, v]) => [k, toKindQuota(v, k)] as const)
        noteQuotaDirty(entries.map(([k]) => k))
        set((s) => ({
          quota: { ...s.quota, ...Object.fromEntries(entries) },
        }))
        if (quotaLimitChanged(before, entries)) void get().refreshState()
        return
      }
      const kind = str(pick(data, 'kind'))
      if (!kind) return
      noteQuotaDirty([kind])
      const quota = toKindQuota(pick(data, 'quota'), kind)
      set((s) => ({ quota: { ...s.quota, [kind]: quota } }))
      if (quotaLimitChanged(before, [[kind, quota]])) void get().refreshState()
      return
    }
    case 'update_restart_changed': {
      if (!isRec(data)) return
      const ids = Array.isArray(data.bot_ids) ? data.bot_ids.filter((v): v is string => typeof v === 'string') : []
      set({ updateRestartScheduled: ids })
      return
    }
    case 'bots_restart_progress': {
      if (!isRec(data)) return
      set((s) => {
        const next = s.restartBatch ? restartProgress(s.restartBatch, data) : null
        return next && next !== s.restartBatch ? { restartBatch: next } : {}
      })
      return
    }
    case 'cli_update_progress': {
      if (!isRec(data)) return
      set((s) => {
        const next = cliUpdateProgressMany(s.cliUpdates, data)
        return next !== s.cliUpdates ? { cliUpdates: next } : {}
      })
      return
    }
    case 'deploy_wait': {
      const n = onDeployWaitFrame(data)
      if (n) get().notify(n.kind, n.text, undefined, n.topCenter)
      return
    }
    case 'herdr_update_progress':
    case 'herdr_update_done': {
      if (!isRec(data)) return
      const n = onHerdrFrame(frame.type, data)
      if (n) {
        get().notify(n.kind, n.text)
        void get().refreshState()
      }
      return
    }
    case 'tool_install_done':
      void handleToolInstallDone(data)
      return
    case 'cli_update_done': {
      if (!isRec(data)) return
      const r = cliUpdateDone(data)
      set((s) => ({
        cliUpdates: cliUpdateDoneMany(s.cliUpdates, data),
        restartBatch: r.batch
          ? s.restartBatch?.id === r.batch.id
            ? s.restartBatch
            : r.batch
          : r.joinBatchId
            ? joinRunningBatch(s.restartBatch, r.joinBatchId)
            : s.restartBatch,
      }))
      get().notify(r.ok ? 'info' : 'error', r.message)
      void get().refreshState()
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
        isRec(v) ? [{ name: str(pick(v, 'name')), error: str(pick(v, 'error'), t('失敗')) }] : [],
      )
      set({
        restartBatch: {
          ...b,
          done: okNames.length + failed.length + skipped.length,
          current: null,
          ok: okNames,
          failed,
          skipped,
          finished: true,
        },
      })
      if (okNames.length + failed.length > 0) {
        const parts = [t('成功 {count} 顆', { count: okNames.length })]
        if (skipped.length > 0) parts.push(t('跳過 {count} 顆', { count: skipped.length }))
        if (failed.length > 0) parts.push(t('失敗 {count} 顆', { count: failed.length }))
        get().notify(failed.length > 0 ? 'error' : 'info', t('更新重啟完成：{parts}', { parts: parts.join(' · ') }))
      }
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
    case 'bot_read': {
      void get().refreshState()
      return
    }
    case 'bot_share_changed': {
      const botId = isRec(data) ? str(pick(data, 'bot_id')) : ''
      if (botId) set((s) => ({ shareRev: { ...s.shareRev, [botId]: (s.shareRev[botId] ?? 0) + 1 } }))
      void get().refreshState()
      return
    }
    case 'bot_changed':
      void get().refreshState()
      return
    case 'composer_draft': {
      const botId = isRec(data) ? str(pick(data, 'bot_id')) : ''
      if (!botId) return
      if (isRec(data) && bool(pick(data, 'present'), false)) {
        void get().loadComposerDraft(botId)
      } else {
        bumpDraftEpoch(botId)
        if (get().composerDrafts[botId]) set((s) => ({ composerDrafts: withoutKey(s.composerDrafts, botId) }))
      }
      return
    }
    default:
      return
  }
}

function dropPane(set: SetFn, host: string, paneId: string) {
  const draftKey = shellDraftKey(host, paneId)
  draftSync.forget(draftKey)
  set((s) => ({
    ...withoutPane(s, host, paneId),
    ...(s.shellView?.host === host && s.shellView.paneId === paneId ? { shellView: null } : {}),
    ...(draftKey in s.drafts ? { drafts: withoutKey(s.drafts, draftKey) } : {}),
  }))
}

function withoutKey<T>(map: Record<string, T>, key: string): Record<string, T> {
  const { [key]: _dropped, ...rest } = map
  return rest
}

const SEND_RETRY_WINDOW_MS = 5 * 60 * 1000

function sendIdIsSpent(turns: Record<string, Turn> | undefined, crid: string): boolean {
  return Object.values(turns ?? {}).some((t) => t.client_request_id === crid && t.status !== 'queued' && t.status !== 'in_flight')
}

function draftWithText(text: string, current: string): string {
  return current === text || (text && (current.startsWith(`${text}\n`) || current.trim() === text.trim()))
    ? current
    : prependDraft(text, current)
}

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
    const identityStatus = u.identities !== undefined ? toIdentityStatusMap(u.identities) : h.identity_status
    const herdr = u.herdr !== undefined ? toHerdrVersion(u.herdr) : h.herdr
    const nextBaseline = u.baseline !== undefined ? toHostBaseline(u.baseline) : h.baseline
    const baseline = JSON.stringify(nextBaseline) === JSON.stringify(h.baseline) ? h.baseline : nextBaseline
    const since = u.disconnected_since !== undefined ? optStr(pick(u, 'disconnected_since')) : connected ? null : h.disconnected_since
    if (connected === h.connected && error === h.error && since === h.disconnected_since && tools === h.tools && identityStatus === h.identity_status && sameHerdr(herdr, h.herdr) && baseline === h.baseline) {
      return h
    }
    changed = true
    return { ...h, connected, error, disconnected_since: since, tools, identity_status: identityStatus, herdr, baseline }
  })
  return changed ? next : current
}

export function hostOfProject(state: StoreState, projectId: string | null): Host | null {
  const project = state.projects.find((p) => p.id === projectId)
  if (!project || project.host === 'local') return null
  return state.hosts.find((h) => h.name === project.host) ?? null
}

export function hostOfBot(state: StoreState, botId: string | null): Host | null {
  const bot = state.bots.find((b) => b.id === botId)
  return bot ? hostOfProject(state, bot.project_id) : null
}

export function projectHostName(state: StoreState, projectId: string | null): string {
  return state.projects.find((p) => p.id === projectId)?.host ?? 'local'
}

export function botLamp(state: StoreState, botId: string): Lamp {
  const host = hostOfBot(state, botId)
  if (host !== null) return lampOf(state.runs[botId], host.connected)
  const bot = state.bots.find((b) => b.id === botId)
  if (bot && projectHostName(state, bot.project_id) !== 'local') return 'disconnected'
  const connected = bot?.herdr_session === 'default' ? state.defaultConnected : state.connected
  return lampOf(state.runs[botId], connected)
}

export type QuotaWarningWindow = '5h' | '7d' | '週'

export interface QuotaWarning {
  critical: boolean
  pct: number
  window: QuotaWarningWindow
}

export function botQuotaWarning(
  quota: QuotaMap,
  kind: BotKind,
  identity: string | null,
  host: string = LOCAL_HOST,
  identities: readonly Identity[] = [],
): QuotaWarning | null {
  const q = quotaForIdentity(quota, host, kind, identity, identities)
  if (!q) return null
  const five = q.five_hour?.critical ? { pct: leftPct(q.five_hour.used_pct), window: '5h' as const } : null
  const sevenWindow: QuotaWarningWindow = weeklyOnlyKind(kind) ? '週' : '7d'
  const seven = q.seven_day?.critical
    ? { pct: leftPct(q.seven_day.used_pct), window: sevenWindow }
    : null
  if (!five && !seven) return null
  const worse = five && seven ? (five.pct <= seven.pct ? five : seven) : (five ?? seven)!
  return { critical: true, pct: worse.pct, window: worse.window }
}

export interface QuotaLevel {
  level: 'warn' | 'crit'
  pct: number
  window: string
}

export function botQuotaLevel(
  quota: QuotaMap,
  kind: BotKind,
  identity: string | null,
  host: string = LOCAL_HOST,
  model: string | null = null,
  identities: readonly Identity[] = [],
): QuotaLevel | null {
  const q = quotaForIdentity(quota, host, kind, identity, identities)
  if (!q) return null
  const onFable = (model ?? '').toLowerCase().includes('fable')
  const pick = (w: { used_pct: number; low: boolean; critical: boolean } | null | undefined, name: string) =>
    w && (w.low || w.critical)
      ? { level: (w.critical ? 'crit' : 'warn') as 'warn' | 'crit', pct: leftPct(w.used_pct), window: name }
      : null
  const hits = [pick(q.five_hour, '5h'), pick(q.seven_day, weeklyOnlyKind(kind) ? '週' : '7d'), onFable ? pick(q.fable, 'F') : null].filter(
    (x): x is QuotaLevel => x !== null,
  )
  if (hits.length === 0) return null
  return hits.sort((a, b) => (a.level === b.level ? a.pct - b.pct : a.level === 'crit' ? -1 : 1))[0]
}

export function orderedProjects(state: { projects: Project[]; projectOrder: string[] }): Project[] {
  if (state.projectOrder.length === 0) return state.projects
  const rank = new Map(state.projectOrder.map((id, i) => [id, i]))
  const known = state.projects.filter((p) => rank.has(p.id)).sort((a, b) => rank.get(a.id)! - rank.get(b.id)!)
  const added = state.projects.filter((p) => !rank.has(p.id))
  return [...known, ...added]
}

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

export function orderedBotIds(state: {
  projects: Project[]
  projectOrder: string[]
  bots: Bot[]
  botOrder: Record<string, string[]>
  hiddenBotIds?: readonly string[]
}): string[] {
  const hidden = new Set(state.hiddenBotIds ?? [])
  const out: string[] = []
  for (const p of orderedProjects(state)) {
    const list = botsOfProject(state, p.id).filter((b) => !hidden.has(b.id))
    const ids = new Set(list.map((b) => b.id))
    const emit = (id: string) => {
      for (const c of list) {
        if (c.parent_bot_id !== id || out.includes(c.id)) continue
        out.push(c.id)
        emit(c.id)
      }
    }
    for (const b of list) {
      if (b.parent_bot_id && ids.has(b.parent_bot_id)) continue
      if (!out.includes(b.id)) out.push(b.id)
      emit(b.id)
    }
  }
  for (const b of state.bots) if (!hidden.has(b.id) && !out.includes(b.id)) out.push(b.id)
  return out
}

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
  if (at < 0) return dir === 1 ? ids[0] : ids[ids.length - 1]
  return ids[(at + dir + ids.length) % ids.length]
}

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
    if (t.delivery === 'unknown' && t.status === 'in_flight') return t
  }
  return null
}

export function composerState(state: StoreState, botId: string | null): ComposerState {
  const base: ComposerState = { disabled: true, reason: '', queued: false, inFlightTurnId: null, unknownTurnId: null }
  if (!botId) return { ...base, reason: t('請先在左側選擇一個 Bot') }
  const bot = state.bots.find((b) => b.id === botId)
  const handedOff = bot ? handedOffTo(state.projects, bot.project_id) : null
  if (handedOff) return { ...base, reason: handedOffReason(handedOff) }
  const hostName = bot ? projectHostName(state, bot.project_id) : 'local'
  const apiBot = isApiKind(bot?.kind)
  if (apiBot) {
  } else if (hostName !== 'local') {
    const host = hostOfBot(state, botId)
    if (!host || !host.connected) {
      return { ...base, reason: t('主機 {hostName} 離線，訊息送不出去', { hostName }) }
    }
  } else if (!(bot?.herdr_session === 'default' ? state.defaultConnected : state.connected)) {
    return { ...base, reason: t('daemon 與 herdr 的連線中斷，無法送出訊息') }
  }
  const run = state.runs[botId]
  const starting = startingSend(state.turns[botId], state.messages[botId])
  if (!run && starting) {
    return { ...base, disabled: false, queued: true, reason: startingSendLabel(starting).replace(/：$/, '') }
  }
  if (!run) {
    if (apiBot) return { ...base, disabled: false, autoStart: true }
    return { ...base, disabled: false, queued: true, autoStart: true, reason: t('Bot 沒在跑：送出會先啟動它，起來後自動送出') }
  }
  if (run.state !== 'running') return { ...base, reason: t('Run 狀態為 {state}，尚無法送出訊息', { state: run.state }) }
  if (run.agent_status === 'blocked') {
    return { ...base, reason: t('agent 正在等待終端回應，請在上方面板按鍵處理') }
  }
  const unknown = unknownDeliveryTurn(state, botId)
  if (unknown) {
    const lock = unknownLock(run.agent_status)
    return { ...base, reason: lock.reason, unknownTurnId: unknown.id, unknownWorking: lock.working }
  }
  const inflight = inFlightTurn(state, botId)
  if (inflight) {
    return { ...base, disabled: false, queued: true, reason: t('這回合還在跑，送出會排到結束後'), inFlightTurnId: inflight.id }
  }
  const queued = Object.values(state.turns[botId] ?? {}).find((t) => t.status === 'queued')
  if (queued) return { ...base, disabled: false, queued: true, reason: t('已有訊息排隊中，等它送出後再試') }
  return { disabled: false, reason: '', queued: false, inFlightTurnId: null, unknownTurnId: null }
}

const NO_TOOLS: ToolMap = toToolMap(undefined)
const NO_IDENTITY_STATUS: IdentityStatusMap = {}

export function toolsOfHost(state: StoreState, host: string): ToolMap {
  if (!host || host === 'local') return state.localTools
  return state.hosts.find((h) => h.name === host)?.tools ?? NO_TOOLS
}

const SHELL_IDENTITY_NAMES = ['cc0', 'cc1', 'cc2', 'cc3', 'cc4', 'cc5', 'cc6']

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
    const detected = Object.keys(status).length > 0
    for (const i of all) {
      if (!hostless(i)) continue
      if (!detected && SHELL_IDENTITY_NAMES.includes(i.name)) continue
      push(i)
    }
  }
  return out
}

export function enabledIdentities(disabled: string[], host: string, list: Identity[]): Identity[] {
  if (disabled.length === 0) return list
  return list.filter((i) => !disabled.includes(api.identityPrefKey(host, i.kind, i.name)))
}

export function quotaClaimantsOf(all: Identity[], status: IdentityStatusMap, disabled: string[], host: string): Identity[] {
  return enabledIdentities(disabled, host, identitiesOfHost(all, status, host))
}

export function quotaClaimants(state: StoreState, host: string): Identity[] {
  return quotaClaimantsOf(state.identities, identityStatusOfHost(state, host), state.disabledIdentities, host)
}

export function kindDisabled(state: StoreState, host: string, kind: string): boolean {
  return state.disabledKinds.includes(api.kindPrefKey(host, kind))
}

export function identityDisabled(state: StoreState, host: string, kind: string, name: string): boolean {
  return state.disabledIdentities.includes(api.identityPrefKey(host, kind, name))
}

export function identityStatusOfHost(state: StoreState, host: string): IdentityStatusMap {
  if (!host || host === 'local') return state.localIdentityStatus
  return state.hosts.find((h) => h.name === host)?.identity_status ?? NO_IDENTITY_STATUS
}

export function missingTools(state: StoreState): { host: string; kind: BotKind }[] {
  const out: { host: string; kind: BotKind }[] = []
  const scan = (host: string, tools: ToolMap) => {
    for (const k of BOT_KINDS) if (!tools[k].installed) out.push({ host, kind: k })
  }
  scan('local', state.localTools)
  for (const h of state.hosts) if (h.connected) scan(h.name, h.tools)
  return out
}

export function runningBotsOnHost(state: StoreState, host: string): Bot[] {
  return state.bots.filter((b) => {
    if (projectHostName(state, b.project_id) !== (host || 'local')) return false
    const run = state.runs[b.id]
    return run !== null && run !== undefined && run.state === 'running'
  })
}

export function liveReplyOf(state: StoreState, botId: string): LiveReply | null {
  const live = state.liveReply[botId]
  if (!live || (!live.text.trim() && !live.activity.trim() && !live.alert.trim())) return null
  const inflight = inFlightTurn(state, botId)
  return inflight && inflight.id === live.turnId ? live : null
}

function composerFocused(key: string): boolean {
  if (typeof document === 'undefined' || !document.hasFocus()) return false
  const el = document.activeElement
  return el instanceof HTMLElement && el.dataset.draftKey === key
}

export const draftSync = new DraftSync({
  clientId: newClientId(),
  put: (key, text, clientId) => api.putDraft(key, text, clientId),
  fetchAll: async () => (await api.fetchDrafts()).filter((draft) => !draft.key.startsWith('group:')),
  read: (key) => useStore.getState().drafts[key] ?? '',
  write: (key, text) =>
    useStore.setState((s) => ({
      drafts: text ? { ...s.drafts, [key]: text } : withoutKey(s.drafts, key),
      draftCursors: text ? s.draftCursors : withoutKey(s.draftCursors, key),
    })),
  focused: composerFocused,
  keys: () => Object.keys(useStore.getState().drafts),
  onRejected: (key, error) => useDraftNotice.getState().mark(key, error),
  onRejectedCleared: (key) => useDraftNotice.getState().clear(key),
})

function newClientId(): string {
  const c = typeof crypto !== 'undefined' ? crypto : undefined
  return c && typeof c.randomUUID === 'function' ? c.randomUUID() : `c-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`
}

if (import.meta.hot) {
  if (prevStoreState && prevStoreState.ready) {
    if (boundSet && boundGet) {
      connectSocket(boundSet, boundGet)
    }
    if (!quotaSweep) quotaSweep = setInterval(() => void useStore.getState().loadQuota(), QUOTA_SWEEP_MS)
  }

  import.meta.hot.dispose((data: Record<string, unknown>) => {
    data.storeState = useStore.getState()
    disconnect?.()
    disconnect = null
    if (quotaSweep) {
      clearInterval(quotaSweep)
      quotaSweep = null
    }
    draftSync.dispose()
  })
}
