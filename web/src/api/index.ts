/**
 * Domain API. Picks the real daemon transport or the in-memory mock
 * (`VITE_MOCK=1`) and exposes typed calls over it.
 */

import { MockTransport } from './mock'
import { toMessagesPage, toState, toTerminal, num, str, isRec, optStr, pick } from './normalize'
import { HttpTransport } from './transport'
import type { SocketHandlers, Transport } from './transport'
import type {
  AppState,
  DirListing,
  HostResult,
  NewHostInput,
  NewIdentityInput,
  MessagesPage,
  NewBotInput,
  NewProjectInput,
  PatchBotInput,
  PatchBotResult,
  PromptResult,
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

export async function sendPrompt(botId: string, text: string, clientRequestId: string): Promise<PromptResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/prompt`, {
    text,
    client_request_id: clientRequestId,
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
