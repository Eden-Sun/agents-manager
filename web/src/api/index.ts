/**
 * Domain API. Picks the real daemon transport or the in-memory mock
 * (`VITE_MOCK=1`) and exposes typed calls over it.
 */

import { MockTransport } from './mock'
import { toGroupMessagesPage, toInstallResult, toIssueDetail, toIssues, toMessagesPage, toModels, toQuota, toState, toTeamDetail, toTeamEvents, toTerminal, num, str, isRec, optStr, pick, arr } from './normalize'
import { HttpTransport } from './transport'
import { ApiError } from './types'
import type { SocketHandlers, Transport } from './transport'
import type {
  AppState,
  Attachment,
  DirListing,
  GroupChatResult,
  GroupMessagesPage,
  GroupSkipReason,
  HostResult,
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
  NewTeamInput,
  PatchBotInput,
  PatchBotResult,
  PatchTeamInput,
  PromptResult,
  BotKind,
  TeamBranchDisposal,
  TeamControlAction,
  TeamDetail,
  TeamEvent,
  TeamTaskDecision,
  TerminalSnapshot,
  TerminalSource,
  TurnDelivery,
} from './types'

export const MOCK_MODE = import.meta.env.VITE_MOCK === '1' || import.meta.env.VITE_MOCK === 'true'

const transport: Transport = MOCK_MODE ? new MockTransport() : new HttpTransport()

export const isMock = transport.mock

export function session(): Promise<string> {
  return transport.session()
}

export function openSocket(handlers: SocketHandlers): () => void {
  return transport.openSocket(handlers)
}

export async function fetchState(): Promise<AppState> {
  return toState(await transport.request('GET', '/state'))
}

export async function fetchMessages(botId: string, limit = 200): Promise<MessagesPage> {
  const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/messages?limit=${limit}`)
  return toMessagesPage(raw, botId)
}

/** SPEC §13.4 `GET /api/projects/:id/messages` — every member bot's messages, merged. */
export async function fetchProjectMessages(projectId: string, limit = 200, before?: string): Promise<GroupMessagesPage> {
  const q = new URLSearchParams({ limit: String(limit) })
  if (before) q.set('before', before)
  const raw = await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/messages?${q.toString()}`)
  return toGroupMessagesPage(raw, projectId)
}

/**
 * SPEC §13.4 `POST /api/projects/:id/chat`. The daemon resolves `@all` / `@<bot>` itself;
 * no valid mention → 400 `{error:"no_mention", bots}` (surfaces as an `ApiError`).
 */
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
        detail: str(pick(x, 'detail', 'message')),
      })),
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
 * `POST /api/bots/:id/pane/move-to-tab` → `200 {}`.
 *
 * 把 bot 現有的 pane 從共用分頁搬到同 workspace 的新分頁（daemon 端對應 herdr 的
 * `pane.move` + `destination.type = "new_tab"`）。同一分頁裡的 pane 互搶寬度，不同分頁不會，
 * 所以這是把窄到讀不了的 pane 救回來的唯一可預期做法（`pane.resize` 是零和的、`zoom` 只放大字）。
 *
 * **搬的是既有 pane，不是重開**：`pane_id` 不變，run / 事件訂閱 / 正在跑的回合都不受影響。
 */
export async function movePaneToTab(botId: string): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/pane/move-to-tab`)
}

/**
 * 「這版 daemon 沒有 `pane/move-to-tab` 這個端點」與真正的失敗分開（docs/FRONTEND.md §8）。
 *
 * 現行 daemon 對 `/api/bots/:id/` 底下的未知路徑回的是**裸 405、空 body**（已實測 2026-09-06），
 * 所以 405 / 501 一律當成沒實作；404 只在**沒有機器碼**時才算——daemon 自己的 404 一定帶
 * `{error, what}`，那是「這個 bot 不見了」，不是「這版沒有這個功能」。
 */
export function isPaneMoveUnsupported(e: unknown): boolean {
  if (!(e instanceof ApiError)) return false
  if (e.status === 405 || e.status === 501) return true
  if (e.status !== 404) return false
  return !e.body.error && !e.body.what
}

/**
 * `GET /api/fs/dirs?host=<name>&path=` — SPEC §11.5. `host` omitted / `"local"` lists the
 * daemon's own filesystem; anything else is listed over ssh on that host.
 */
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

// ------------------------------------------------------------------ hosts (§11.6)

function toHostResult(raw: unknown, name: string): HostResult {
  const o = isRec(raw) ? raw : {}
  return {
    name: str(pick(o, 'name'), name),
    connected: o.connected === true || o.ok === true,
    error: optStr(pick(o, 'error', 'last_error', 'message', 'reason')),
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
  return isRec(raw) ? str(pick(raw, 'project_id', 'id')) : ''
}

export async function deleteProject(projectId: string): Promise<void> {
  await transport.request('DELETE', `/projects/${encodeURIComponent(projectId)}`)
}

export async function createIdentity(input: NewIdentityInput): Promise<void> {
  await transport.request('POST', '/identities', input)
}

export async function deleteIdentity(name: string): Promise<void> {
  await transport.request('DELETE', `/identities/${encodeURIComponent(name)}`)
}

export async function createBot(projectId: string, input: NewBotInput): Promise<string> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/bots`, input)
  return isRec(raw) ? str(pick(raw, 'bot_id', 'id')) : ''
}

/**
 * `PATCH /api/bots/:id` (API.md v3.3). Only the changed fields are sent; `model` /
 * `identity` as `null` clears them. Renaming a bot that has an active Run is a 409.
 */
export async function patchBot(botId: string, input: PatchBotInput): Promise<PatchBotResult> {
  const raw = await transport.request('PATCH', `/bots/${encodeURIComponent(botId)}`, input)
  const o = isRec(raw) ? raw : {}
  return { needs_restart: o.needs_restart === true || o.restart_required === true }
}

/** `POST /api/bots/:id/restart` — stop then start; returns the new run id. */
export async function restartBot(botId: string): Promise<string> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restart`)
  return isRec(raw) ? str(pick(raw, 'run_id')) : ''
}

export async function deleteBot(botId: string): Promise<void> {
  await transport.request('DELETE', `/bots/${encodeURIComponent(botId)}`)
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

export async function sendPrompt(
  botId: string,
  text: string,
  clientRequestId: string,
  attachments: string[] = [],
): Promise<PromptResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/prompt`, {
    text,
    client_request_id: clientRequestId,
    ...(attachments.length ? { attachments } : {}),
  })
  const o = isRec(raw) ? raw : {}
  const delivery = str(pick(o, 'delivery'), 'pending') as TurnDelivery
  return {
    turn_id: str(pick(o, 'turn_id')),
    message_id: str(pick(o, 'message_id')) || null,
    delivery,
  }
}

export async function sendKeys(botId: string, keys: string[], expectRunId: string | null): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/keys`, {
    keys,
    ...(expectRunId ? { expect_run_id: expectRunId } : {}),
  })
}

export async function abandonTurn(turnId: string): Promise<void> {
  await transport.request('POST', `/turns/${encodeURIComponent(turnId)}/abandon`)
}

// ---------------------------------------------------------- attachments

/**
 * `POST /api/bots/:id/attachments?name=…` with the raw image as the body. The daemon puts
 * the file on the bot's host and returns the metadata the prompt needs.
 */
export async function uploadAttachment(botId: string, file: File): Promise<Attachment> {
  const qs = new URLSearchParams({ name: file.name || 'image' })
  const raw = await transport.upload(`/bots/${encodeURIComponent(botId)}/attachments?${qs.toString()}`, file)
  const o = isRec(raw) ? raw : {}
  return {
    id: str(pick(o, 'id')),
    name: str(pick(o, 'name'), file.name || 'image'),
    mime: str(pick(o, 'mime'), file.type || 'image/png'),
    size: num(pick(o, 'size'), file.size),
    path: str(pick(o, 'path')),
  }
}

/** An object URL for a stored attachment (the bytes sit behind the UI token). */
export function attachmentUrl(id: string): Promise<string> {
  return transport.blobUrl(`/attachments/${encodeURIComponent(id)}`)
}

// ------------------------------------------------------------------ v4.0

/** `GET /api/models?kind=&host=` — the agent CLI's model catalogue on that host. */
export async function fetchModels(kind: BotKind, host?: string): Promise<ModelInfo[]> {
  const q = new URLSearchParams({ kind })
  if (host && host !== 'local') q.set('host', host)
  return toModels(await transport.request('GET', `/models?${q.toString()}`))
}

/** `GET /api/quota` — per-kind 5h / 7d usage. */
export async function fetchQuota(): Promise<QuotaMap> {
  return toQuota(await transport.request('GET', '/quota'))
}

/**
 * `POST /api/hosts/:name/tools/install {kind, via_bot_id}` — asks a running bot on that
 * host to install + log in the given agent CLI (as a prompt in its own pane).
 */
export async function installTool(host: string, kind: BotKind, viaBotId: string): Promise<InstallToolResult> {
  return toInstallResult(
    await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/tools/install`, { kind, via_bot_id: viaBotId }),
  )
}

/** `GET /api/projects/:id/issues?state=&limit=&q=` (v4.0; the daemon shells out to `gh`). */
export async function fetchIssues(projectId: string, opts: { state?: 'open' | 'closed'; limit?: number; q?: string } = {}): Promise<Issue[]> {
  const q = new URLSearchParams()
  if (opts.state) q.set('state', opts.state)
  if (opts.limit) q.set('limit', String(opts.limit))
  if (opts.q) q.set('q', opts.q)
  const qs = q.toString()
  return toIssues(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/issues${qs ? `?${qs}` : ''}`))
}

/** `GET /api/projects/:id/issues/:number` — with the full body. */
export async function fetchIssue(projectId: string, number: number): Promise<IssueDetail | null> {
  return toIssueDetail(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/issues/${number}`))
}

// -------------------------------------------------------- Issue Team（SPEC-team §10）

/**
 * 這批端點在舊 daemon 上並不存在。呼叫端一律用 `isTeamsUnsupported()` 判斷，
 * 把「daemon 還沒有 team」跟真正的錯誤分開（docs/FRONTEND.md §8：缺端點要靜默退回）。
 */
export function isTeamsUnsupported(e: unknown): boolean {
  if (!(e instanceof ApiError)) return false
  if (e.status === 405 || e.status === 501) return true
  if (e.status !== 404) return false
  // daemon 自己的 404 一定帶機器碼（`{error:"not_found", what:"team"}`）——那是「這個 team
  // 不見了」，不是「這版 daemon 沒有 team」。路由根本不存在時回的是裸 404。
  return !e.body.error && !e.body.what
}

/** `POST /api/projects/:id/teams` → `{team_id}`。 */
export async function createTeam(projectId: string, input: NewTeamInput): Promise<string> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/teams`, input)
  return isRec(raw) ? str(pick(raw, 'team_id', 'id')) : ''
}

/** `GET /api/teams/:id` — team 物件 + tasks + base/worktree。 */
export async function fetchTeam(teamId: string): Promise<TeamDetail | null> {
  return toTeamDetail(await transport.request('GET', `/teams/${encodeURIComponent(teamId)}`), teamId)
}

/** `GET /api/teams/:id/events?before=&limit=` — 倒序分頁、正序回傳。 */
export async function fetchTeamEvents(teamId: string, limit = 100, before?: string): Promise<TeamEvent[]> {
  const q = new URLSearchParams({ limit: String(limit) })
  if (before) q.set('before', before)
  return toTeamEvents(await transport.request('GET', `/teams/${encodeURIComponent(teamId)}/events?${q.toString()}`))
}

/** `POST /api/teams/:id/{pause|resume|approve|abort|cleanup}`。 */
export async function controlTeam(teamId: string, action: TeamControlAction, body?: unknown): Promise<void> {
  await transport.request('POST', `/teams/${encodeURIComponent(teamId)}/${action}`, body)
}

/**
 * `DELETE /api/teams/:id?branches=keep|delete`（SPEC-team §6.5a）。
 *
 * 與 `cleanup` 不同：**任何 phase 都可以刪**（非終態時等於先 abort 再刪），而且連
 * `teams` / `team_tasks` / `team_events` 的紀錄一起移除。成員的對話訊息保留。
 * 冪等：team 不存在回 `404 {error:"not_found", what:"team"}`，呼叫端當成功處理。
 */
export async function deleteTeam(teamId: string, branches: TeamBranchDisposal = 'keep'): Promise<void> {
  await transport.request('DELETE', `/teams/${encodeURIComponent(teamId)}?branches=${branches}`)
}

/** daemon 回的「這個 team 不存在」（與「這版 daemon 沒有 team 端點」不同，見 `isTeamsUnsupported`）。 */
export function isTeamNotFound(e: unknown): boolean {
  return e instanceof ApiError && e.status === 404 && e.body.what === 'team'
}

/** `PATCH /api/teams/:id {budget?, supervised?, deliver?}`。 */
export async function patchTeam(teamId: string, input: PatchTeamInput): Promise<void> {
  await transport.request('PATCH', `/teams/${encodeURIComponent(teamId)}`, input)
}

/** `POST /api/teams/:id/say {text, to, client_request_id}` — 使用者插話（不計預算）。 */
export async function sayToTeam(teamId: string, text: string, to: string, clientRequestId: string): Promise<void> {
  await transport.request('POST', `/teams/${encodeURIComponent(teamId)}/say`, { text, to, client_request_id: clientRequestId })
}

/** `POST /api/teams/:id/answer {text}` — 回覆 PM 的 `ask_user`（等同 say + resume）。 */
export async function answerTeam(teamId: string, text: string): Promise<void> {
  await transport.request('POST', `/teams/${encodeURIComponent(teamId)}/answer`, { text })
}

/** `POST /api/teams/:id/tasks/:tid/decide {action, note?}`。 */
export async function decideTeamTask(
  teamId: string,
  taskId: string,
  action: TeamTaskDecision,
  note?: string,
): Promise<void> {
  await transport.request('POST', `/teams/${encodeURIComponent(teamId)}/tasks/${encodeURIComponent(taskId)}/decide`, {
    action,
    ...(note ? { note } : {}),
  })
}

/** `crypto.randomUUID()` with a fallback for non-secure origins. */
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
