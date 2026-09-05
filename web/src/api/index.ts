/**
 * Domain API. Picks the real daemon transport or the in-memory mock
 * (`VITE_MOCK=1`) and exposes typed calls over it.
 */

import { MockTransport } from './mock'
import { toMessagesPage, toState, toTerminal, num, str, isRec, pick } from './normalize'
import { HttpTransport } from './transport'
import type { SocketHandlers, Transport } from './transport'
import type {
  AppState,
  DirListing,
  MessagesPage,
  NewBotInput,
  NewProjectInput,
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

export async function listDirs(path?: string): Promise<DirListing> {
  const q = path ? `?path=${encodeURIComponent(path)}` : ''
  const raw = await transport.request('GET', `/fs/dirs${q}`)
  const r = isRec(raw) ? raw : {}
  const entries = Array.isArray(r.entries) ? r.entries : []
  return {
    path: str(r.path),
    parent: r.parent == null ? null : str(r.parent),
    home: str(r.home),
    entries: entries.filter(isRec).map((e) => ({ name: str(e.name), path: str(e.path), git: e.git === true })),
  }
}

export async function createProject(input: NewProjectInput): Promise<string> {
  const raw = await transport.request('POST', '/projects', input)
  return isRec(raw) ? str(pick(raw, 'project_id', 'id')) : ''
}

export async function deleteProject(projectId: string): Promise<void> {
  await transport.request('DELETE', `/projects/${encodeURIComponent(projectId)}`)
}

export async function createBot(projectId: string, input: NewBotInput): Promise<string> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/bots`, input)
  return isRec(raw) ? str(pick(raw, 'bot_id', 'id')) : ''
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
