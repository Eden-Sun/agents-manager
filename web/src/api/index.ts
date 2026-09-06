/**
 * Domain API. Picks the real daemon transport or the in-memory mock
 * (`VITE_MOCK=1`) and exposes typed calls over it.
 */

import { MockTransport } from './mock'
import { toGroupMessagesPage, toInstallResult, toIssueDetail, toIssues, toMessagesPage, toModels, toQuota, toState, toTerminal, num, str, isRec, optStr, pick, arr } from './normalize'
import { HttpTransport } from './transport'
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
  PatchBotInput,
  PatchBotResult,
  PromptResult,
  BotKind,
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
 * `GET /api/fs/dirs?host=<name>&path=` — SPEC §11.5. `host` omitted / `"local"` lists the
 * daemon's own filesystem; anything else is listed over ssh on that host.
 */
export async function listDirs(path?: string, host?: string): Promise<DirListing> {
  const params = new URLSearchParams()
  if (path) params.set('path', path)
  if (host && host !== 'local') params.set('host', host)
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
