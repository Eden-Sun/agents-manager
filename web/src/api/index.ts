import { MockTransport } from './mock'
import { toMission, toMissionDetail, toMissionEvent, toMissions, toGroupMessagesPage, toHostShell, toHostShells, toInstallResult, toIssueDetail, toIssues, toMessagesPage, toMemProcesses, toMemSnapshot, toModels, toQuota, toState, toTerminal, toToolMap, toIdentityStatusMap, num, str, isRec, optStr, pick, arr, toSubmodules } from './normalize'
import { HttpTransport } from './transport'
import { ApiError } from './types'
import { parseTriageRows } from '../lib/releaseTriage'
import type { SocketHandlers, Transport, UploadOptions } from './transport'
import type {
  MemProcesses,
  MemSnapshot,
  MessageHit,
  ModelRemap,
  AppState,
  Attachment,
  DirListing,
  GhLoginMode,
  GhStatus,
  GroupChatResult,
  Mission,
  MissionDetail,
  MissionEvent,
  NewMissionInput,
  GroupMessagesPage,
  GroupSkipReason,
  HostResult,
  HostShell,
  UpdateReview,
  RemoteCargoInput,
  RemoteCargoSettings,
  IdentityStatusMap,
  InstallToolResult,
  Issue,
  IssueDetail,
  ModelInfo,
  QuotaMap,
  NewHostInput,
  NewIdentityInput,
  MessagesPage,
  NewBotInput,
  NewProjectInput,
  PatchProjectInput,
  PatchBotInput,
  PatchBotResult,
  PromptResult,
  RestartPlan,
  RestartSkip,
  BotKind,
  TerminalSnapshot,
  TerminalSource,
  ToolMap,
  TurnDelivery,
ProjectSubmodule,
} from './types'

export const MOCK_MODE = import.meta.env.VITE_MOCK === '1' || import.meta.env.VITE_MOCK === 'true'

const transport: Transport = MOCK_MODE ? new MockTransport() : new HttpTransport()

export const isMock = transport.mock

/** 獨立 API 模組共用這條；各自 `new` transport 會在 mock 模式變成兩份不相干的假資料。 */
export const rawTransport: Pick<Transport, 'request' | 'mock'> = transport

export function session(): Promise<string> {
  return transport.session()
}

export function openSocket(handlers: SocketHandlers): () => void {
  return transport.openSocket(handlers)
}

export async function fetchState(): Promise<AppState> {
  return toState(await transport.request('GET', '/state'))
}

/** API.md §6: `before` = 目前最舊一則的 id（往前翻，issue #25）。 */
export async function fetchMessages(
  botId: string,
  limit = 200,
  before?: string,
  /** API.md §6：只要某個回合（或某個 role）的訊息，不必翻整段歷史。 */
  filter?: { turnId?: string; role?: 'user' | 'assistant' | 'system' },
): Promise<MessagesPage> {
  const q = new URLSearchParams({ limit: String(limit) })
  if (before) q.set('before', before)
  if (filter?.turnId) q.set('turn_id', filter.turnId)
  if (filter?.role) q.set('role', filter.role)
  const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/messages?${q.toString()}`)
  return toMessagesPage(raw, botId)
}

/** SPEC §13.4 — every member bot's messages, merged. */
export async function fetchProjectMessages(projectId: string, limit = 200, before?: string): Promise<GroupMessagesPage> {
  const q = new URLSearchParams({ limit: String(limit) })
  if (before) q.set('before', before)
  const raw = await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/messages?${q.toString()}`)
  return toGroupMessagesPage(raw, projectId)
}

/** SPEC §13.4. Daemon resolves mentions; none valid → 400 `no_mention` (`ApiError`). */
export async function sendGroupChat(
  projectId: string,
  text: string,
  clientRequestId: string,
  attachments: string[] = [],
): Promise<GroupChatResult> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/chat`, {
    text,
    client_request_id: clientRequestId,
    ...(attachments.length ? { attachments } : {}),
  })
  const o = isRec(raw) ? raw : {}
  return {
    group_id: str(pick(o, 'group_id'), clientRequestId),
    sent: arr(o.sent)
      .filter(isRec)
      .map((x) => ({
        bot_id: str(pick(x, 'bot_id')),
        bot_name: str(pick(x, 'bot_name')),
        turn_id: str(pick(x, 'turn_id')),
        message_id: str(pick(x, 'message_id')) || null,
        delivery: str(pick(x, 'delivery'), 'pending') as TurnDelivery,
      })),
    skipped: arr(o.skipped)
      .filter(isRec)
      .map((x) => ({
        bot_id: str(pick(x, 'bot_id')),
        bot_name: str(pick(x, 'bot_name')),
        reason: str(pick(x, 'reason'), 'conflict') as GroupSkipReason,
        detail: str(pick(x, 'detail')),
      })),
  }
}

/** SPEC §6.5e：專案底下的非 agent pane（shell／服務）。 */
export interface ProjectPane {
  pane_id: string
  host: string
  workspace_id: string | null
  tab_id: string | null
  cwd: string | null
  kind: 'shell' | 'service'
  owned_by: 'bot' | 'user' | 'none'
  owner_bot_id: string | null
  project_id: string | null
  purpose: string | null
  foreground: string | null
  listen_ports: number[]
  last_output_at: string
  first_seen: string
  last_seen: string
  gc_optin: boolean
  /** daemon 判的「只能看」（有 listen port）。舊 daemon 沒這欄＝`undefined`，呼叫端退回看 port（`lib/shellAccess`）。 */
  read_only?: boolean
  /** 只在 `GET /api/panes?unowned=1`：daemon 認定的那顆固定 scratch。舊 daemon 沒這欄＝`undefined`。 */
  scratch?: boolean
}

function toPane(v: unknown): ProjectPane {
  const r = (v ?? {}) as Record<string, unknown>
  const str = (x: unknown): string => (typeof x === 'string' ? x : '')
  const opt = (x: unknown): string | null => (typeof x === 'string' && x ? x : null)
  return {
    pane_id: str(r.pane_id),
    host: str(r.host) || 'local',
    workspace_id: opt(r.workspace_id),
    tab_id: opt(r.tab_id),
    cwd: opt(r.cwd),
    kind: r.kind === 'service' ? 'service' : 'shell',
    owned_by: r.owned_by === 'bot' || r.owned_by === 'user' ? r.owned_by : 'none',
    owner_bot_id: opt(r.owner_bot_id),
    project_id: opt(r.project_id),
    purpose: opt(r.purpose),
    foreground: opt(r.foreground),
    listen_ports: Array.isArray(r.listen_ports) ? r.listen_ports.filter((p): p is number => typeof p === 'number') : [],
    last_output_at: str(r.last_output_at),
    first_seen: str(r.first_seen),
    last_seen: str(r.last_seen),
    gc_optin: r.gc_optin === true,
    ...(typeof r.read_only === 'boolean' ? { read_only: r.read_only } : {}),
    ...(typeof r.scratch === 'boolean' ? { scratch: r.scratch } : {}),
  }
}

export async function fetchProjectPanes(projectId: string): Promise<ProjectPane[]> {
  const raw = await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/panes`)
  const list = (raw as { panes?: unknown[] })?.panes
  return Array.isArray(list) ? list.map(toPane) : []
}

/** 全部被 trace 的 pane（`GET /api/panes`）：重整／深連結時用來把「從選單點進去的 pane」還原回來。 */
export async function fetchAllPanes(): Promise<ProjectPane[]> {
  const raw = await transport.request('GET', '/panes')
  const list = (raw as { panes?: unknown[] })?.panes
  return Array.isArray(list) ? list.map(toPane) : []
}

/** 對不到任何專案的 pane（`GET /api/panes?unowned=1`），側欄底部那一組；daemon 會標哪一顆是 scratch。 */
export async function fetchUnownedPanes(): Promise<ProjectPane[]> {
  const raw = await transport.request('GET', '/panes?unowned=1')
  const list = (raw as { panes?: unknown[] })?.panes
  return Array.isArray(list) ? list.map(toPane) : []
}

/** 關服務 pane 沒帶 confirm 的 409：回 daemon 附上的那一列（最新的 kind／port），拿來問人。其他錯誤回 `null`。 */
export function servicePaneConflict(e: unknown): ProjectPane | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'service_pane') return null
  const pane = toPane(e.body.pane)
  return pane.pane_id ? pane : null
}

/**
 * 關 pane 被擋下要人再確認：在 listen 的服務 pane，或 daemon 讀不到它現在在跑什麼（`unverified`）。
 * 帶著 daemon 附上的那一列回來，UI 才講得出「正在 listen 3010」或「讀不到狀態」。
 */
export interface CloseNeedsConfirm {
  pane: ProjectPane | null
  unverified: boolean
}

export function closeNeedsConfirm(e: unknown): CloseNeedsConfirm | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'service_pane') return null
  const pane = toPane(e.body.pane)
  return { pane: pane.pane_id ? pane : null, unverified: e.body.unverified === true }
}

export async function focusPane(paneId: string, host: string): Promise<void> {
  await transport.request('POST', `/panes/${encodeURIComponent(paneId)}/focus?host=${encodeURIComponent(host)}`)
}

/** 服務 pane 要 `confirm`（UI 先顯示 port 再問一次）；沒帶會回 409。 */
export async function closePane(paneId: string, host: string, confirm: boolean): Promise<void> {
  const q = `host=${encodeURIComponent(host)}${confirm ? '&confirm=true' : ''}`
  await transport.request('POST', `/panes/${encodeURIComponent(paneId)}/close?${q}`)
}

/** claude 停在 AskUserQuestion 時 transcript 裡的題目（`lib/pendingQuestion.ts`）。舊 daemon 沒這支（404／405）就當沒有。 */
export async function fetchPendingQuestion(botId: string): Promise<unknown> {
  try {
    const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/pending-question`)
    return isRec(raw) ? raw.questions : null
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405)) return null
    throw e
  }
}

export async function fetchTerminal(
  botId: string,
  source: TerminalSource,
  lines: number,
): Promise<TerminalSnapshot> {
  const raw = await transport.request(
    'GET',
    `/bots/${encodeURIComponent(botId)}/terminal?source=${source}&lines=${lines}`,
  )
  return toTerminal(raw, source)
}

/**
 * 搬到新分頁是救回窄 pane 唯一可預期的做法（`pane.resize` 零和、`zoom` 只放大字）。
 * 搬既有 pane 不重開：`pane_id`、run、進行中回合都不受影響。
 */
/** 跨裝置共用的已讀位置：只往前推，daemon 會推 `bot_read` 給其他分頁／裝置。 */
export async function markBotRead(botId: string, mark: { at: string; id: string }): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/read`, { at: mark.at, message_id: mark.id })
}

export async function movePaneToTab(botId: string): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/pane/move-to-tab`)
}

/** SPEC §11.5. Non-local `host` is listed over ssh. */
export async function listDirs(path?: string, host?: string, hidden?: boolean): Promise<DirListing> {
  const params = new URLSearchParams()
  if (path) params.set('path', path)
  if (host && host !== 'local') params.set('host', host)
  if (hidden) params.set('hidden', '1')
  const q = params.toString()
  const raw = await transport.request('GET', `/fs/dirs${q ? `?${q}` : ''}`)
  const r = isRec(raw) ? raw : {}
  const entries = Array.isArray(r.entries) ? r.entries : []
  return {
    path: str(r.path),
    parent: r.parent == null ? null : str(r.parent),
    home: str(r.home),
    entries: entries.filter(isRec).map((e) => ({ name: str(e.name), path: str(e.path), git: e.git === true })),
  }
}

// hosts (SPEC §11.6)

function toHostResult(raw: unknown, name: string): HostResult {
  const o = isRec(raw) ? raw : {}
  return {
    name: str(pick(o, 'name'), name),
    connected: o.connected === true,
    error: optStr(pick(o, 'error')),
  }
}

export async function createHost(input: NewHostInput): Promise<HostResult> {
  return toHostResult(await transport.request('POST', '/hosts', input), input.name)
}

export async function deleteHost(name: string): Promise<void> {
  await transport.request('DELETE', `/hosts/${encodeURIComponent(name)}`)
}

export async function reconnectHost(name: string): Promise<HostResult> {
  return toHostResult(await transport.request('POST', `/hosts/${encodeURIComponent(name)}/reconnect`), name)
}

export async function createProject(input: NewProjectInput): Promise<string> {
  const raw = await transport.request('POST', '/projects', input)
  return isRec(raw) ? str(pick(raw, 'project_id')) : ''
}

/** API.md §3. Rename never blocked by a live run: label only affects the next start's slug. */
export async function patchProject(projectId: string, input: PatchProjectInput): Promise<void> {
  await transport.request('PATCH', `/projects/${encodeURIComponent(projectId)}`, input)
}

/** 停用名單的鍵：身份是每台主機、每個 kind 各一份（cc1 在別台是別的帳號）。 */
export function identityPrefKey(host: string, kind: string, name: string): string {
  return `${host || 'local'}|${kind}|${name}`
}

/** 目前被標為停用的身份。停用只影響「還挑不挑得到它」，不會動已經綁著它的 bot。 */
export async function fetchDisabledIdentities(): Promise<string[]> {
  const raw = await transport.request('GET', '/identity-prefs')
  const rows = isRec(raw) ? pick(raw, 'disabled') : null
  if (!Array.isArray(rows)) return []
  return rows
    .filter(isRec)
    .map((r) => identityPrefKey(str(pick(r, 'host')), str(pick(r, 'kind')), str(pick(r, 'identity'))))
}

export async function setIdentityDisabled(host: string, kind: string, name: string, disabled: boolean): Promise<void> {
  await transport.request('PUT', `/identities/${encodeURIComponent(name)}/disabled`, {
    kind,
    disabled,
    host: host || 'local',
  })
}

/** bot 放進 `$AM_OUTBOX` 交給使用者的檔案（SPEC §6.5f）。`reason` 有值＝這顆沒有可列的（遠端）。
 *  `remainingSecs`：daemon 讀清單那一刻離被清掉還剩幾秒（放進去 1 小時後清）。 */
export type OutboxFile = { name: string; size: number; modified: number; remainingSecs: number }

export async function fetchOutbox(
  botId: string,
): Promise<{ dir: string; reason: string | null; ttlSecs: number; files: OutboxFile[] }> {
  const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/outbox`)
  const o = isRec(raw) ? raw : {}
  const rows = pick(o, 'files')
  const files = Array.isArray(rows)
    ? rows.filter(isRec).map((r) => ({
        name: str(pick(r, 'name')),
        size: num(pick(r, 'size')),
        modified: num(pick(r, 'modified')),
        remainingSecs: num(pick(r, 'remaining_secs')),
      }))
    : []
  return {
    dir: str(pick(o, 'dir')),
    reason: optStr(pick(o, 'reason')),
    ttlSecs: num(pick(o, 'ttl_secs')),
    files: files.filter((f) => f.name),
  }
}

/** 下載 outbox 裡的一個檔案：帶 token 抓成 blob URL（`<a href>` 帶不了 header）。 */
export function outboxFileUrl(botId: string, name: string): Promise<string> {
  return transport.blobUrl(`/bots/${encodeURIComponent(botId)}/outbox/file?path=${encodeURIComponent(name)}`)
}

export async function deleteProject(projectId: string): Promise<void> {
  await transport.request('DELETE', `/projects/${encodeURIComponent(projectId)}`)
}

export async function createIdentity(input: NewIdentityInput): Promise<void> {
  await transport.request('POST', '/identities', input)
}

/** 刪的是**那一台**的那一筆：同名的 `cc1` 在別台是別的帳號（SPEC §16.2）。 */
export async function deleteIdentity(name: string, host = 'local'): Promise<void> {
  const q = host && host !== 'local' ? `?host=${encodeURIComponent(host)}` : ''
  await transport.request('DELETE', `/identities/${encodeURIComponent(name)}${q}`)
}

/** API.md「停用模型的回應」（#400）：`{"remapped":{"model":{"from","to"}}}`；沒換就沒有這一段。 */
function toModelRemap(v: unknown): ModelRemap | null {
  if (!isRec(v)) return null
  const m = isRec(v.model) ? v.model : null
  if (!m) return null
  const from = str(pick(m, 'from'))
  const to = str(pick(m, 'to'))
  return from && to && from !== to ? { from, to } : null
}

export async function createBot(projectId: string, input: NewBotInput): Promise<{ id: string; name: string; remapped_model: ModelRemap | null }> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/bots`, input)
  const o = isRec(raw) ? raw : {}
  return { id: str(pick(o, 'bot_id')), name: str(pick(o, 'name')), remapped_model: toModelRemap(o.remapped) }
}

/** API.md v3.3. `null` `model`/`identity` clears; renaming with an active Run → 409. */
export async function patchBot(botId: string, input: PatchBotInput): Promise<PatchBotResult> {
  const raw = await transport.request('PATCH', `/bots/${encodeURIComponent(botId)}`, input)
  const o = isRec(raw) ? raw : {}
  const la = isRec(o.live_apply) ? o.live_apply : null
  return {
    needs_restart: o.needs_restart === true || o.restart_required === true,
    ...(la ? { live_apply: { applied: la.applied === true, deferred: la.deferred === true, reason: typeof la.reason === 'string' ? la.reason : null } } : {}),
    remapped_model: toModelRemap(o.remapped),
  }
}

/** 側欄排序（API.md §5.4），存 config.toml 讓各裝置共用同一份；`primary` 是主力那列的順序（#344，存 DB）。 */
export async function saveOrder(
  input: { projects?: string[]; bots?: Record<string, string[]>; primary?: string[] },
  signal?: AbortSignal,
): Promise<void> {
  await transport.request('POST', '/order', input, signal)
}

/** API.md §fork：從頂層 bot 分出新 bot，CLI 接續來源的對話脈絡。建好但沒起來時 `start_error` 有值。 */
export interface ForkResult {
  id: string
  name: string
  start_error: string | null
}

export async function forkBot(botId: string, name?: string): Promise<ForkResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/fork`, name ? { name } : {})
  const o = isRec(raw) ? raw : {}
  const err = pick(o, 'start_error')
  return { id: str(pick(o, 'bot_id')), name: str(pick(o, 'name')), start_error: typeof err === 'string' && err ? err : null }
}

/** API.md §promote：子 agent 升級成頂層 bot，保留同一段 claude 對話。 */
export interface PromoteResult {
  id: string
  name: string
}

export async function promoteBot(botId: string, name?: string): Promise<PromoteResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/promote`, name ? { name } : {})
  const o = isRec(raw) ? raw : {}
  return { id: str(pick(o, 'bot_id')), name: str(pick(o, 'name')) }
}

/**
 * `resumeNative`：帶 `?resume=native` 接回 DB 記的原生對話（換身分後重啟要接續，SPEC §6.5.2）。
 * 接不回時 daemon 回 409 `cannot_resume`——呼叫端（`store.restartBot`）決定要不要改成不帶這個旗標重送。
 */
export async function restartBot(botId: string, resumeNative?: boolean): Promise<string> {
  const qs = resumeNative ? '?resume=native' : ''
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restart${qs}`)
  return isRec(raw) ? str(pick(raw, 'run_id')) : ''
}

/**
 * `POST /bots/:id/rewind`（SPEC §6.13）：在終端驅動 claude 的 `/rewind`，倒回到這則使用者訊息送出之前；
 * `text` 是那則的原文（回填輸入框用）；`pane_cleared: false`＝CLI 放回終端輸入列的那段沒清掉。
 */
export async function rewindBot(botId: string, messageId: string): Promise<{ text: string; hidden: number; paneCleared: boolean }> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/rewind`, { message_id: messageId })
  const o = isRec(raw) ? raw : {}
  return { text: str(pick(o, 'text')), hidden: num(pick(o, 'hidden'), 0), paneCleared: pick(o, 'pane_cleared') !== false }
}

/** SPEC §6.9. 只回計畫；實際重啟在背景跑，進度走 WS `bots_restart_progress` / `bots_restart_done`。 */
export async function restartIdleBots(): Promise<RestartPlan> {
  const raw = await transport.request('POST', '/bots/restart-idle')
  const o = isRec(raw) ? raw : {}
  return {
    batch_id: str(pick(o, 'batch_id')),
    total: num(pick(o, 'total'), 0),
    planned: arr(pick(o, 'planned')).flatMap((v) =>
      isRec(v) ? [{ bot_id: str(pick(v, 'bot_id')), name: str(pick(v, 'name')) }] : [],
    ),
    skipped: toRestartSkips(pick(o, 'skipped')),
    already_running: pick(o, 'already_running') === true,
  }
}

/** WS 事件與 REST 回應共用。 */
export function toRestartSkips(v: unknown): RestartSkip[] {
  return arr(v).flatMap((x) =>
    isRec(x)
      ? [
          {
            bot_id: str(pick(x, 'bot_id')),
            name: str(pick(x, 'name')),
            reason: str(pick(x, 'reason')),
            reason_label: str(pick(x, 'reason_label')),
          },
        ]
      : [],
  )
}

/** `confirmSupervisor`：刪 AGM 的 bot 要明講（daemon 否則 409 `supervisor_owned`，issue #406）。 */
export async function deleteBot(botId: string, opts: { confirmSupervisor?: boolean } = {}): Promise<void> {
  await transport.request('DELETE', `/bots/${encodeURIComponent(botId)}${opts.confirmSupervisor ? '?confirm=supervisor' : ''}`)
}

export async function startBot(botId: string): Promise<string> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/start`)
  return isRec(raw) ? str(pick(raw, 'run_id')) : ''
}

export async function stopBot(botId: string): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/stop`)
}

export async function interruptBot(botId: string): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/interrupt`)
}

/** 強制結束回合：不同於 `interrupt`（esc 送不出就失敗），這支先保證解鎖，送鍵只是順帶。 */
export async function abortBot(botId: string): Promise<{ aborted: string[]; keys_sent: boolean }> {
  const r = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/abort`)
  const o = isRec(r) ? r : {}
  return {
    aborted: Array.isArray(o.aborted) ? o.aborted.map((x) => String(x)) : [],
    keys_sent: o.keys_sent === true,
  }
}

/** 對執行中 bot 送 `/login`；只送指令，完成與否要靠 `refreshTools` 重新偵測。 */
export async function loginBot(botId: string): Promise<{ command: string; kind: string }> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/login`)
  const o = isRec(raw) ? raw : {}
  return { command: str(pick(o, 'command'), '/login'), kind: str(pick(o, 'kind')) }
}

/** `sendNow`＝插隊送出（issue #103）：對方回合中時打斷它，而不是回 409。只有 claude ≥ 2.1.275
 *  的 run 認得那顆鍵，其他情況 daemon 照舊 409，body 帶 `send_now_refused`。
 *  `startIfStopped`＝bot 沒在跑時 daemon 先收下（`delivery: queued`）再自己啟動它（issue #122）。 */
export async function sendPrompt(
  botId: string,
  text: string,
  clientRequestId: string,
  attachments: string[] = [],
  sendNow = false,
  startIfStopped = false,
): Promise<PromptResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/prompt`, {
    text,
    client_request_id: clientRequestId,
    ...(attachments.length ? { attachments } : {}),
    ...(sendNow ? { send_now: true } : {}),
    ...(startIfStopped ? { start_if_stopped: true } : {}),
  })
  const o = isRec(raw) ? raw : {}
  const delivery = str(pick(o, 'delivery'), 'pending') as PromptResult['delivery']
  return {
    turn_id: str(pick(o, 'turn_id')),
    message_id: str(pick(o, 'message_id')) || null,
    delivery,
    send_now: str(pick(o, 'send_now')) || null,
  }
}

export async function sendKeys(botId: string, keys: string[], expectRunId: string | null): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/keys`, {
    keys,
    ...(expectRunId ? { expect_run_id: expectRunId } : {}),
  })
}

/** 多行文字走這裡，不要拆成 `sendKeys` 的鍵名。 */
export async function sendText(botId: string, text: string, enter: boolean, expectRunId: string | null): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/text`, {
    text,
    enter,
    ...(expectRunId ? { expect_run_id: expectRunId } : {}),
  })
}

export async function abandonTurn(turnId: string): Promise<void> {
  await transport.request('POST', `/turns/${encodeURIComponent(turnId)}/abandon`)
}

/** issue #122：撤回一則 bot 沒在跑時送、還在等它起來的訊息。已經送出去的 daemon 回 409。 */
export async function withdrawTurn(turnId: string): Promise<void> {
  await transport.request('POST', `/turns/${encodeURIComponent(turnId)}/withdraw`)
}

export async function uploadAttachment(botId: string, file: File, opts?: UploadOptions): Promise<Attachment> {
  const qs = new URLSearchParams({ name: file.name || 'image' })
  const raw = await transport.upload(`/bots/${encodeURIComponent(botId)}/attachments?${qs.toString()}`, file, opts)
  const o = isRec(raw) ? raw : {}
  return {
    id: str(pick(o, 'id')),
    name: str(pick(o, 'name'), file.name || 'image'),
    mime: str(pick(o, 'mime'), file.type || 'image/png'),
    size: num(pick(o, 'size'), file.size),
    path: str(pick(o, 'path')),
  }
}

export function attachmentUrl(id: string): Promise<string> {
  return transport.blobUrl(`/attachments/${encodeURIComponent(id)}`)
}

/** 對話裡 Markdown 圖片的本機路徑（相對於 bot 的專案目錄）；專案外、非圖片一律 404。 */
export function localImageUrl(botId: string, path: string): Promise<string> {
  return transport.blobUrl(`/bots/${encodeURIComponent(botId)}/local-image?path=${encodeURIComponent(path)}`)
}

/** `identity` (claude) picks whose settings.json the default effort comes from (SPEC §17.1); unknown falls back safely. */
export async function fetchModels(kind: BotKind, host?: string, identity?: string | null): Promise<ModelInfo[]> {
  const q = new URLSearchParams({ kind })
  if (host && host !== 'local') q.set('host', host)
  if (identity) q.set('identity', identity)
  return toModels(await transport.request('GET', `/models?${q.toString()}`))
}

export async function fetchQuota(): Promise<QuotaMap> {
  return toQuota(await transport.request('GET', '/quota'))
}

/** 失敗就丟出來，由呼叫端當成「沒有訊息命中」。 */
export async function searchMessages(q: string): Promise<Record<string, MessageHit>> {
  const raw = await transport.request('GET', `/search/messages?q=${encodeURIComponent(q)}`)
  const r = isRec(raw) ? raw : {}
  const out: Record<string, MessageHit> = {}
  for (const row of Array.isArray(r.bots) ? r.bots : []) {
    if (!isRec(row)) continue
    const id = str(row.bot_id)
    if (id) out[id] = { hits: num(row.hits, 0), snippet: str(row.snippet) }
  }
  return out
}

/** 軟刪復原。失敗丟 `ApiError`（撞名 409、不存在 404），由呼叫端顯示，不能吞成 false。 */
export async function restoreBot(botId: string): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restore`)
}

/** SPEC §15. */
export async function fetchMem(): Promise<MemSnapshot> {
  return toMemSnapshot(await transport.request('GET', '/mem'))
}

/** SPEC §15.2. */
export async function fetchMemPane(host: string, paneId: string, socket: string | null, lines = 40): Promise<TerminalSnapshot> {
  const sock = socket ? `&socket=${encodeURIComponent(socket)}` : ''
  const raw = await transport.request(
    'GET',
    `/mem/processes/pane?host=${encodeURIComponent(host)}&pane_id=${encodeURIComponent(paneId)}${sock}&lines=${lines}`,
  )
  return toTerminal(raw, 'visible')
}

/** SPEC §15.2. */
export async function fetchMemProcesses(host: string): Promise<MemProcesses> {
  return toMemProcesses(await transport.request('GET', `/mem/processes?host=${encodeURIComponent(host)}`))
}

/** SPEC §15.2. daemon 擋 herdr 本身（400）與 bot（409）；錯誤原樣上丟，照 daemon 訊息顯示。 */
export async function killMemProcess(host: string, pid: number, signal: 'TERM' | 'KILL' = 'TERM'): Promise<void> {
  await transport.request('POST', '/mem/processes/kill', { host, pid, signal })
}

/** Detection otherwise only runs on host (re)connect — needed right after logging in. */
export async function refreshTools(host: string): Promise<{ tools: ToolMap; identity_status: IdentityStatusMap }> {
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/tools/refresh`)
  const rec = isRec(raw) ? raw : {}
  return { tools: toToolMap(rec.tools), identity_status: toIdentityStatusMap(rec.identities) }
}

function toRemoteCargoSettings(raw: unknown): RemoteCargoSettings {
  // 舊 daemon 沒有這條路徑時 GET 會落到 SPA、回 200 + index.html；當成空物件會靜靜顯示一組假預設值。
  if (!isRec(raw)) throw new ApiError(404, {}, '這顆 daemon 還沒有 /api/build/remote（回的是網頁不是 JSON），二進位比前端舊；要 cargo build --release 後重啟 daemon。')
  const o = raw
  return {
    enabled: o.enabled === true,
    host: str(o.host),
    user: str(o.user),
    ssh_port: num(o.ssh_port, 22),
    remote_root: str(o.remote_root, '.cache/agents-manager/remote-cargo'),
    cargo_jobs: num(o.cargo_jobs, 4),
    password_set: o.password_set === true,
  }
}

export async function fetchRemoteCargoSettings(): Promise<RemoteCargoSettings> {
  return toRemoteCargoSettings(await transport.request('GET', '/build/remote'))
}

export async function saveRemoteCargoSettings(input: RemoteCargoInput): Promise<RemoteCargoSettings> {
  return toRemoteCargoSettings(await transport.request('PUT', '/build/remote', input))
}

export async function testRemoteCargo(): Promise<{
  ok: boolean
  output: string
  password_auth: boolean
  cargo_missing: boolean
  cargo_version: string
  /** 有 cargo 卻沒有 clippy（`cargo clippy` 轉過去會失敗；按安裝會補上）。 */
  clippy_missing: boolean
  os: string
  arch: string
}> {
  const raw = await transport.request('POST', '/build/remote/test', {})
  const o = isRec(raw) ? raw : {}
  return {
    ok: o.ok === true,
    output: str(o.output),
    password_auth: o.password_auth === true,
    cargo_missing: o.cargo_missing === true,
    cargo_version: str(o.cargo_version),
    clippy_missing: o.clippy_missing === true,
    os: str(o.os),
    arch: str(o.arch),
  }
}

/** 在遠端裝 Rust 工具鏈（rustup minimal）。已經有就只回現有版本。 */
export async function installRemoteCargoToolchain(): Promise<{
  already_installed: boolean
  cargo_version: string
  cc_missing: boolean
  clippy_missing: boolean
  output: string
}> {
  const raw = await transport.request('POST', '/build/remote/install-toolchain', {})
  const o = isRec(raw) ? raw : {}
  return {
    already_installed: o.already_installed === true,
    cargo_version: str(o.cargo_version),
    cc_missing: o.cc_missing === true,
    clippy_missing: o.clippy_missing === true,
    output: str(o.output),
  }
}

function toReview(raw: unknown): UpdateReview {
  const o = isRec(raw) ? raw : {}
  const state = o.state === 'done' || o.state === 'pending' ? o.state : 'none'
  return {
    state,
    target_bot_name: str(o.target_bot_name),
    asked_at: str(o.asked_at),
    answered_at: str(o.answered_at),
    result: str(o.result),
  }
}

/**
 * 這一版的 AGM 解析到哪了（更新框一打開就讀，有結論就直接顯示）。`version` 是 daemon 實際查的那一版：
 * claude 沒給 `to` 時就是磁碟上那一版，更新框拿它當分診區間的終點。
 */
export async function fetchUpdateReview(input: {
  kind: string
  host?: string
  to?: string | null
}): Promise<{ version: string; review: UpdateReview }> {
  const q = new URLSearchParams({ kind: input.kind })
  if (input.host) q.set('host', input.host)
  if (input.to) q.set('to', input.to)
  const raw = await transport.request('GET', `/claude-update/review?${q}`)
  const o = isRec(raw) ? raw : {}
  return { version: str(o.version), review: toReview(o.review) }
}

/** 請 AGM 解析這一版的 changelog（claude／codex；唯讀，只建交辦）。 */
export async function requestUpdateReview(input: { kind: string; host?: string; from?: string | null; to?: string | null }): Promise<{
  version: string
  target_bot_name: string
  duplicate: boolean
}> {
  const raw = await transport.request('POST', '/claude-update/review', {
    kind: input.kind,
    host: input.host,
    from: input.from ?? undefined,
    to: input.to ?? undefined,
  })
  const o = isRec(raw) ? raw : {}
  return { version: str(o.version), target_bot_name: str(o.target_bot_name), duplicate: o.duplicate === true }
}

/** 上游新版分診帳本（SPEC §18.2c）：更新框顯示這幾版分析過了沒、結論是什麼（issue #561）。 */
export async function fetchReleaseTriage(kind: string): Promise<ReturnType<typeof parseTriageRows>> {
  return parseTriageRows(await transport.request('GET', `/release-triage?kind=${encodeURIComponent(kind)}`))
}

/** 開臨時 pane 做該身份的登入。 */
export async function loginIdentity(host: string, identity: string): Promise<HostShell> {
  return identityAuth(host, identity, 'login')
}

/** 同一條路的登出：帶著這個身份自己的設定目錄下指令，不會動到別的帳號。 */
export async function logoutIdentity(host: string, identity: string): Promise<HostShell> {
  return identityAuth(host, identity, 'logout')
}

async function identityAuth(host: string, identity: string, op: 'login' | 'logout'): Promise<HostShell> {
  const name = host || 'local'
  const raw = await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(name)}/identities/${encodeURIComponent(identity)}/${op}`,
  )
  return toHostShell(raw, name)
}

/** Asks a running bot on that host to install + log in the CLI via a prompt. */
export async function installTool(host: string, kind: BotKind, viaBotId: string): Promise<InstallToolResult> {
  return toInstallResult(
    await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/tools/install`, { kind, via_bot_id: viaBotId }),
  )
}

function toGhStatus(raw: unknown, fallbackName: string): GhStatus {
  const o = isRec(raw) ? raw : {}
  const pendingRaw = isRec(o.pending) ? o.pending : null
  return {
    name: str(pick(o, 'name'), fallbackName),
    installed: o.installed === true,
    path: optStr(pick(o, 'path')),
    logged_in: o.logged_in === true,
    account: optStr(pick(o, 'account')),
    accounts: arr(o.accounts)
      .filter(isRec)
      .map((a) => ({
        login: str(pick(a, 'login')),
        active: a.active === true,
        ok: a.ok === true,
      }))
      .filter((a) => a.login),
    mode: optStr(pick(o, 'mode')),
    pending: pendingRaw
      ? {
          user_code: str(pick(pendingRaw, 'user_code')),
          verification_uri: str(pick(pendingRaw, 'verification_uri'), 'https://github.com/login/device'),
          verification_uri_complete: optStr(pick(pendingRaw, 'verification_uri_complete')),
          expires_in: num(pick(pendingRaw, 'expires_in'), 0),
        }
      : null,
    error: optStr(pick(o, 'error')),
  }
}

export async function fetchGhStatus(host: string): Promise<GhStatus> {
  const name = host || 'local'
  return toGhStatus(await transport.request('GET', `/hosts/${encodeURIComponent(name)}/gh`), name)
}

export async function loginGh(host: string, mode: GhLoginMode = 'auto', user?: string): Promise<GhStatus> {
  const name = host || 'local'
  return toGhStatus(
    await transport.request('POST', `/hosts/${encodeURIComponent(name)}/gh/login`, {
      mode,
      ...(user ? { user } : {}),
    }),
    name,
  )
}

export async function cancelGhLogin(host: string): Promise<GhStatus> {
  const name = host || 'local'
  return toGhStatus(await transport.request('POST', `/hosts/${encodeURIComponent(name)}/gh/cancel`), name)
}

/** 純 shell pane。`pane_id` 是白名單 key，只在這輪 daemon 有效，重啟後舊 pane 不認。 */
export async function openHostShell(host: string, cwd?: string): Promise<HostShell> {
  const name = host || 'local'
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(name)}/shells`, cwd ? { cwd } : {})
  return toHostShell(raw, name)
}

export async function fetchHostShells(host: string): Promise<HostShell[]> {
  const name = host || 'local'
  return toHostShells(await transport.request('GET', `/hosts/${encodeURIComponent(name)}/shells`), name)
}

export async function readHostShell(
  host: string,
  paneId: string,
  source: TerminalSource,
  lines: number,
): Promise<TerminalSnapshot> {
  const raw = await transport.request(
    'GET',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/terminal?source=${source}&lines=${lines}`,
  )
  return toTerminal(raw, source)
}

/** `enter` 是獨立按鍵而非 `\n`（herdr 把換行當貼上）；空字串 + `enter` = 只按 Enter。 */
export async function sendHostShellText(host: string, paneId: string, text: string, enter = true): Promise<void> {
  await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/text`,
    { text, enter },
  )
}

export async function sendHostShellKeys(host: string, paneId: string, keys: string[]): Promise<void> {
  await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/keys`,
    { keys },
  )
}

/** 已經沒了也算成功。 */
/** `confirm` 只在人看過 port／「讀不到狀態」之後才帶；沒帶時服務 pane 與讀不到的 pane 會回 409 `service_pane`。 */
export async function closeHostShell(host: string, paneId: string, confirm = false): Promise<void> {
  await transport.request(
    'DELETE',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}${confirm ? '?confirm=true' : ''}`,
  )
}

/** Daemon shells out to `gh`. */
export async function fetchIssues(
  projectId: string,
  opts: { state?: 'open' | 'closed'; limit?: number; q?: string; repo?: string } = {},
): Promise<Issue[]> {
  const q = new URLSearchParams()
  if (opts.repo) q.set('repo', opts.repo)
  if (opts.state) q.set('state', opts.state)
  if (opts.limit) q.set('limit', String(opts.limit))
  if (opts.q) q.set('q', opts.q)
  const qs = q.toString()
  return toIssues(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/issues${qs ? `?${qs}` : ''}`))
}

export async function fetchIssue(projectId: string, number: number, repo = ''): Promise<IssueDetail | null> {
  const qs = repo ? `?repo=${encodeURIComponent(repo)}` : ''
  return toIssueDetail(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/issues/${number}${qs}`))
}

export async function fetchSubmodules(projectId: string): Promise<ProjectSubmodule[]> {
  return toSubmodules(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/submodules`))
}

/** Fallback: `randomUUID` is missing on non-secure origins. */
export function newClientRequestId(): string {
  const c = globalThis.crypto
  if (c && typeof c.randomUUID === 'function') return c.randomUUID()
  const bytes = new Uint8Array(16)
  if (c && typeof c.getRandomValues === 'function') c.getRandomValues(bytes)
  else for (let i = 0; i < 16; i++) bytes[i] = Math.floor(Math.random() * 256)
  bytes[6] = (bytes[6] & 0x0f) | 0x40
  bytes[8] = (bytes[8] & 0x3f) | 0x80
  const hex = [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('')
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`
}

export { num }
export type { SocketHandlers }

// 快速 git（chat 標題列 chip，2026-09-08）
export interface GitSummary {
  git: boolean
  branch: string | null
  upstream: string | null
  ahead: number
  behind: number
  changed: number
  untracked: number
  insertions: number
  deletions: number
}

/** 404（專案不在）→ `{git:false}`，chip 靜默消失。 */
export async function fetchGit(projectId: string): Promise<GitSummary> {
  const none: GitSummary = { git: false, branch: null, upstream: null, ahead: 0, behind: 0, changed: 0, untracked: 0, insertions: 0, deletions: 0 }
  let raw: unknown
  try {
    raw = await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/git`)
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return none
    throw e
  }
  const o = isRec(raw) ? raw : {}
  if (pick(o, 'git') !== true) return none
  return {
    git: true,
    branch: optStr(pick(o, 'branch')),
    upstream: optStr(pick(o, 'upstream')),
    ahead: num(pick(o, 'ahead')),
    behind: num(pick(o, 'behind')),
    changed: num(pick(o, 'changed')),
    untracked: num(pick(o, 'untracked')),
    insertions: num(pick(o, 'insertions')),
    deletions: num(pick(o, 'deletions')),
  }
}

/** commit = `git add -A && git commit`；pull = `--rebase --no-autostash`。 */
export async function gitAction(projectId: string, op: 'commit' | 'push' | 'pull', message?: string): Promise<string> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/git/${op}`, op === 'commit' ? { message } : undefined)
  const o = isRec(raw) ? raw : {}
  return str(pick(o, 'output'), '')
}

// 群組任務：見 docs/API.md「群組任務」

/** 同 `client_request_id` 回同一筆（冪等）；遠端專案 → 400 `remote_not_supported`。 */
export async function createMission(
  projectId: string,
  input: NewMissionInput,
): Promise<{ mission: Mission | null; created: boolean }> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/missions`, input)
  const root = isRec(raw) ? raw : {}
  return { mission: toMission(pick(root, 'mission') ?? raw), created: pick(root, 'created') !== false }
}

/** 這一筆任務不在（daemon 的結構化 404，`docs/API.md`「群組任務」）。 */
export function isMissionGone(e: unknown): boolean {
  return e instanceof ApiError && e.status === 404 && e.body.error === 'not_found'
}

/**
 * 舊 daemon 沒這些路由（SPA fallback 會回 index.html）。
 * 404 兩種都有：body 認得出是 daemon 的結構化錯誤就代表路由在、只是那一筆不見了——
 * 把它一起當成「這台不支援」會因為一張過期的任務卡把整個群組任務功能靜默關掉。
 */
export function isMissionsUnsupported(e: unknown): boolean {
  if (!(e instanceof ApiError)) return false
  return e.status === 405 || e.status === 501 || (e.status === 404 && !isMissionGone(e))
}

export function missionRejectReason(e: unknown): string | null {
  return e instanceof ApiError ? (e.body?.error ?? null) : null
}

/** `open` 含 paused。已結案的要自己帶 `limit`，進行中的別跟它們擠同一個上限（見 store 的 `loadMissions`）。 */
export async function fetchMissions(
  projectId: string,
  status: 'all' | 'open' | 'done' | 'cancelled',
  limit: number,
): Promise<Mission[]> {
  const qs = new URLSearchParams({ status, limit: String(limit) }).toString()
  return toMissions(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/missions?${qs}`))
}

export async function fetchMission(missionId: string): Promise<MissionDetail | null> {
  return toMissionDetail(await transport.request('GET', `/missions/${encodeURIComponent(missionId)}`))
}

/** 契約：不帶 `relay_from` ＝ 使用者本人說的；bot 轉述才帶自己的 bot id。 */
export async function addMissionEvent(
  missionId: string,
  input: { kind: 'report' | 'note' | 'verified'; text: string; relay_from?: string; payload?: unknown },
): Promise<MissionEvent | null> {
  const raw = await transport.request('POST', `/missions/${encodeURIComponent(missionId)}/events`, input)
  const root = isRec(raw) ? raw : {}
  return toMissionEvent(pick(root, 'event') ?? raw, missionId)
}

/** 追問只留言並叫醒 AGM，不改狀態（要改用 `reviseMission`）；`client_request_id` 冪等。 */
export async function askMission(
  missionId: string,
  input: { text: string; client_request_id: string },
): Promise<MissionEvent | null> {
  const raw = await transport.request('POST', `/missions/${encodeURIComponent(missionId)}/question`, input)
  const root = isRec(raw) ? raw : {}
  return toMissionEvent(pick(root, 'event') ?? raw, missionId)
}

/** daemon 端單一交易；別自己串 events + resume（中途斷掉會留半套）。 */
export async function answerMission(
  missionId: string,
  input: { text: string; client_request_id: string },
): Promise<{ mission: Mission | null; resumed: boolean }> {
  const raw = await transport.request('POST', `/missions/${encodeURIComponent(missionId)}/answer`, input)
  const root = isRec(raw) ? raw : {}
  return { mission: toMission(pick(root, 'mission')), resumed: pick(root, 'resumed') === true }
}

/** 回新任務（`parent_mission_id` 指回來），原成果不動。 */
export async function reviseMission(
  missionId: string,
  input: { text: string; client_request_id: string },
): Promise<Mission | null> {
  const raw = await transport.request('POST', `/missions/${encodeURIComponent(missionId)}/revise`, input)
  return toMission(isRec(raw) ? (pick(raw, 'mission') ?? raw) : raw)
}

export async function controlMission(
  missionId: string,
  action: 'pause' | 'resume' | 'cancel',
  body?: unknown,
): Promise<Mission | null> {
  const raw = await transport.request('POST', `/missions/${encodeURIComponent(missionId)}/${action}`, body ?? {})
  const root = isRec(raw) ? raw : {}
  return toMission(pick(root, 'mission') ?? raw)
}
