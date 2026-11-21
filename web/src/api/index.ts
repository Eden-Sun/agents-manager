import { MockTransport } from './mock'
import { toHostBaselinePlan, toHostBaselineApplyResult, toHostBaselineBackups, toHostBaselineRestoreResult, toHostShell, toHostShells, toInstallResult, toIssueDetail, toIssues, toMessagesPage, toMemProcesses, toMemSnapshot, toModelsResponse, toQuota, toState, toTerminal, toToolMap, toIdentityStatusMap, num, str, isRec, optStr, pick, arr, toSubmodules, type ModelsResponse } from './normalize'
import { HttpTransport } from './transport'
import { ApiError } from './types'
import { parseTriageRows } from '../lib/releaseTriage'
import type { SocketHandlers, StreamHandlers, Transport, UploadOptions } from './transport'
import type {
  MemProcesses,
  MemSnapshot,
  MessageHit,
  ModelRemap,
  AppState,
  Attachment,
  CreatedDir,
  DirListing,
  GhLoginMode,
  GhStatus,
  HostBaselinePlan,
  HostBaselineApplyResult,
  HostBaselineBackups,
  HostBaselineRestoreResult,
  HostResult,
  HostShell,
  UpdateReview,
  RemoteCargoInput,
  RemoteCargoSettings,
  IdentityStatusMap,
  InstallToolResult,
  Issue,
  IssueDetail,
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
  WithdrawTurnResult,
  RestartPlan,
  RestartSkip,
  BotKind,
  TerminalFormat,
  TerminalSnapshot,
  TerminalSource,
  ToolMap,
ProjectSubmodule,
} from './types'
import { t, langFromSearch, setLangOverride } from '../lib/i18n'

export const MOCK_MODE = import.meta.env.VITE_MOCK === '1' || import.meta.env.VITE_MOCK === 'true'

if (MOCK_MODE && typeof location !== 'undefined') {
  const forced = langFromSearch(location.search)
  if (forced) setLangOverride(forced)
}

const transport: Transport = MOCK_MODE ? new MockTransport() : new HttpTransport()

export const isMock = transport.mock

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

export async function fetchMessages(
  botId: string,
  limit = 200,
  before?: string,
  filter?: { turnId?: string; role?: 'user' | 'assistant' | 'system' },
): Promise<MessagesPage> {
  const q = new URLSearchParams({ limit: String(limit) })
  if (before) q.set('before', before)
  if (filter?.turnId) q.set('turn_id', filter.turnId)
  if (filter?.role) q.set('role', filter.role)
  const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/messages?${q.toString()}`)
  return toMessagesPage(raw, botId)
}

export interface ProjectPane {
  alias?: string | null
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
  read_only?: boolean
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
    alias: opt(r.alias),
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

export async function fetchAllPanes(): Promise<ProjectPane[]> {
  const raw = await transport.request('GET', '/panes')
  const list = (raw as { panes?: unknown[] })?.panes
  return Array.isArray(list) ? list.map(toPane) : []
}

export async function fetchUnownedPanes(): Promise<ProjectPane[]> {
  const raw = await transport.request('GET', '/panes?unowned=1')
  const list = (raw as { panes?: unknown[] })?.panes
  return Array.isArray(list) ? list.map(toPane) : []
}

export function servicePaneConflict(e: unknown): ProjectPane | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'service_pane') return null
  const pane = toPane(e.body.pane)
  return pane.pane_id ? pane : null
}

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

export async function closePane(paneId: string, host: string, confirm: boolean): Promise<void> {
  const q = `host=${encodeURIComponent(host)}${confirm ? '&confirm=true' : ''}`
  await transport.request('POST', `/panes/${encodeURIComponent(paneId)}/close?${q}`)
}

export async function fetchPendingQuestion(botId: string): Promise<unknown> {
  try {
    const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/pending-question`)
    return isRec(raw) ? raw : null
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405)) return null
    throw e
  }
}

export interface ComposerDraftSnapshot {
  state: 'draft' | 'empty' | 'unready' | 'dialog'
  dialog: string
  runId: string
  busy: boolean
  draft: string
  token: string
  actions: Array<'submit' | 'clear'>
}

export async function fetchComposerDraft(botId: string): Promise<ComposerDraftSnapshot> {
  const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/composer-draft`)
  const o = isRec(raw) ? raw : {}
  const state = pick(o, 'state')
  const actions = Array.isArray(pick(o, 'draft_actions')) ? (pick(o, 'draft_actions') as unknown[]) : []
  return {
    state: state === 'draft' || state === 'unready' || state === 'dialog' ? state : 'empty',
    dialog: isRec(pick(o, 'dialog')) ? str(pick(pick(o, 'dialog') as Record<string, unknown>, 'text')) : '',
    runId: str(pick(o, 'run_id')),
    busy: pick(o, 'busy') === true,
    draft: str(pick(o, 'draft')),
    token: str(pick(o, 'draft_token')),
    actions: (['submit', 'clear'] as const).filter((x) => actions.includes(x)),
  }
}

export async function fetchTerminal(
  botId: string,
  source: TerminalSource,
  lines: number,
  format: TerminalFormat = 'text',
): Promise<TerminalSnapshot> {
  const fmt = format === 'ansi' ? '&format=ansi' : ''
  const raw = await transport.request(
    'GET',
    `/bots/${encodeURIComponent(botId)}/terminal?source=${source}&lines=${lines}${fmt}`,
  )
  const snap = toTerminal(raw, source)
  return format === 'ansi' ? { ...snap, format: 'ansi' } : snap
}

export async function markBotRead(botId: string, mark: { at: string; id: string }): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/read`, { at: mark.at, message_id: mark.id })
}

export async function movePaneToTab(botId: string): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/pane/move-to-tab`)
}

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
    truncated: r.truncated === true,
  }
}

export async function createDir(parent: string, name: string, host?: string): Promise<CreatedDir> {
  const body: Record<string, string> = { parent, name }
  if (host && host !== 'local') body.host = host
  const raw = await transport.request('POST', '/fs/dirs', body)
  const r = isRec(raw) ? raw : {}
  return { path: str(r.path), name: str(r.name), parent: r.parent == null ? null : str(r.parent) }
}

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

export async function patchProject(projectId: string, input: PatchProjectInput): Promise<void> {
  await transport.request('PATCH', `/projects/${encodeURIComponent(projectId)}`, input)
}

export function identityPrefKey(host: string, kind: string, name: string): string {
  return `${host || 'local'}|${kind}|${name}`
}

export async function fetchDisabledIdentities(): Promise<string[]> {
  const raw = await transport.request('GET', '/identity-prefs')
  const rows = isRec(raw) ? pick(raw, 'disabled') : null
  if (!Array.isArray(rows)) return []
  return rows
    .filter(isRec)
    .map((r) => identityPrefKey(str(pick(r, 'host')), str(pick(r, 'kind')), str(pick(r, 'identity'))))
}

export async function fetchDisabledKinds(): Promise<string[]> {
  const raw = await transport.request('GET', '/kind-prefs')
  const rows = isRec(raw) ? pick(raw, 'disabled') : null
  if (!Array.isArray(rows)) return []
  return rows.filter(isRec).map((r) => kindPrefKey(str(pick(r, 'host')), str(pick(r, 'kind'))))
}

export function kindPrefKey(host: string, kind: string): string {
  return `${host || 'local'}|${kind}`
}

export async function setKindDisabled(host: string, kind: string, disabled: boolean): Promise<void> {
  await transport.request('PUT', `/kinds/${encodeURIComponent(kind)}/disabled`, { host: host || 'local', disabled })
}

export async function setIdentityDisabled(host: string, kind: string, name: string, disabled: boolean): Promise<void> {
  await transport.request('PUT', `/identities/${encodeURIComponent(name)}/disabled`, {
    kind,
    disabled,
    host: host || 'local',
  })
}

export type OutboxFile = {
  name: string
  size: number
  modified: number
  remainingSecs: number | null
  owner?: string
  ownerName?: string
}

export async function fetchOutbox(
  botId: string,
): Promise<{ dir: string; reason: string | null; ttlSecs: number; kept: boolean; files: OutboxFile[] }> {
  const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/outbox`)
  const o = isRec(raw) ? raw : {}
  const rows = pick(o, 'files')
  const files = Array.isArray(rows)
    ? rows.filter(isRec).map((r) => ({
        name: str(pick(r, 'name')),
        size: num(pick(r, 'size')),
        modified: num(pick(r, 'modified')),
        remainingSecs: typeof pick(r, 'remaining_secs') === 'number' ? num(pick(r, 'remaining_secs')) : null,
      }))
    : []
  return {
    dir: str(pick(o, 'dir')),
    reason: optStr(pick(o, 'reason')),
    ttlSecs: num(pick(o, 'ttl_secs')),
    kept: pick(o, 'kept') === true,
    files: files.filter((f) => f.name),
  }
}

export async function outboxFileUrl(botId: string, name: string, inline = false): Promise<string> {
  const q = inline ? '&inline=1' : ''
  const url = await transport.blobUrl(`/bots/${encodeURIComponent(botId)}/outbox/file?path=${encodeURIComponent(name)}${q}`)
  return inline ? utf8PreviewUrl(url) : url
}

export async function outboxHtmlUrl(botId: string, name: string): Promise<string> {
  const raw = await outboxFileUrl(botId, name, false)
  try {
    const blob = await (await fetch(raw)).blob()
    return URL.createObjectURL(new Blob([blob], { type: 'text/html;charset=utf-8' }))
  } finally {
    URL.revokeObjectURL(raw)
  }
}

export async function utf8PreviewUrl(url: string): Promise<string> {
  const blob = await (await fetch(url)).blob()
  if (!blob.type.startsWith('text/')) return url
  const head = new Uint8Array(await blob.slice(0, 3).arrayBuffer())
  if (head[0] === 0xef && head[1] === 0xbb && head[2] === 0xbf) return url
  URL.revokeObjectURL(url)
  return URL.createObjectURL(new Blob([new Uint8Array([0xef, 0xbb, 0xbf]), blob], { type: 'text/plain;charset=utf-8' }))
}

export async function deleteProject(projectId: string): Promise<void> {
  await transport.request('DELETE', `/projects/${encodeURIComponent(projectId)}`)
}

export async function createIdentity(input: NewIdentityInput): Promise<void> {
  await transport.request('POST', '/identities', input)
}

export async function deleteIdentity(name: string, host = 'local'): Promise<void> {
  const q = host && host !== 'local' ? `?host=${encodeURIComponent(host)}` : ''
  await transport.request('DELETE', `/identities/${encodeURIComponent(name)}${q}`)
}

export interface ShellIdentityDeleteResponse {
  ok: true
  host: string
  kind: 'claude'
  identity: string
  backup_path: string
  warning: 'external_editor_window'
}

export async function deleteShellIdentity(host: string, identity: string): Promise<ShellIdentityDeleteResponse> {
  const name = host || 'local'
  const raw = await transport.request(
    'DELETE',
    `/hosts/${encodeURIComponent(name)}/shell-identities/${encodeURIComponent(identity)}?kind=claude`,
  )
  const o = isRec(raw) ? raw : {}
  const backupPath = typeof o.backup_path === 'string' ? o.backup_path : undefined
  if (o.ok !== true || o.host !== name || o.kind !== 'claude' || o.identity !== identity
    || !backupPath?.trim() || !backupPath.startsWith('/') || o.warning !== 'external_editor_window') {
    throw new ApiError(502, {
      error: 'upstream', reason: 'alias_delete_unconfirmed', backup_path: backupPath,
    }, 'alias_delete_unconfirmed')
  }
  return {
    ok: true,
    host: name,
    kind: 'claude',
    identity,
    backup_path: backupPath,
    warning: 'external_editor_window',
  }
}

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

export async function patchBot(botId: string, input: PatchBotInput): Promise<PatchBotResult> {
  const raw = await transport.request('PATCH', `/bots/${encodeURIComponent(botId)}`, input)
  const o = isRec(raw) ? raw : {}
  const la = isRec(o.live_apply) ? o.live_apply : null
  return {
    needs_restart: o.needs_restart === true || o.restart_required === true,
    ...(la ? { live_apply: { applied: la.applied === true, deferred: la.deferred === true, interrupted: la.interrupted === true, reason: typeof la.reason === 'string' ? la.reason : null } } : {}),
    remapped_model: toModelRemap(o.remapped),
    message: optStr(pick(o, 'message')),
  }
}

export async function saveOrder(
  input: { projects?: string[]; bots?: Record<string, string[]>; primary?: string[] },
  signal?: AbortSignal,
): Promise<void> {
  await transport.request('POST', '/order', input, signal)
}

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

export interface PromoteResult {
  id: string
  name: string
}

export async function promoteBot(botId: string, name?: string): Promise<PromoteResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/promote`, name ? { name } : {})
  const o = isRec(raw) ? raw : {}
  return { id: str(pick(o, 'bot_id')), name: str(pick(o, 'name')) }
}

export async function restartBot(botId: string, resumeNative?: boolean): Promise<string> {
  const qs = resumeNative ? '?resume=native' : ''
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restart${qs}`)
  return isRec(raw) ? str(pick(raw, 'run_id')) : ''
}

export async function rewindBot(
  botId: string,
  messageId: string,
  clearComposer?: string,
): Promise<{ text: string; hidden: number; paneCleared: boolean }> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/rewind`, {
    message_id: messageId,
    ...(clearComposer !== undefined ? { clear_composer: true, expect_composer: clearComposer } : {}),
  })
  const o = isRec(raw) ? raw : {}
  return { text: str(pick(o, 'text')), hidden: num(pick(o, 'hidden'), 0), paneCleared: pick(o, 'pane_cleared') !== false }
}

export async function translateMessage(
  messageId: string,
  force = false,
  retranslate = false,
): Promise<{ translation: string; model: string; cached: boolean; costUsd: number | null; lowConfidence: boolean; jevProb: number | null }> {
  const query = retranslate ? '?retranslate=true' : force ? '?force=true' : ''
  const raw = await transport.request('POST', `/messages/${encodeURIComponent(messageId)}/translate${query}`)
  const o = isRec(raw) ? raw : {}
  const cost = pick(o, 'cost_usd')
  const jev = pick(o, 'jev_prob')
  return {
    translation: str(pick(o, 'translation')),
    model: str(pick(o, 'model')),
    cached: pick(o, 'cached') === true,
    costUsd: typeof cost === 'number' ? cost : null,
    lowConfidence: pick(o, 'low_confidence') === true,
    jevProb: typeof jev === 'number' ? jev : null,
  }
}

export async function reportTranslation(messageId: string): Promise<{ sent: boolean; reason: string | null; sentAt: string | null }> {
  const raw = await transport.request('POST', `/messages/${encodeURIComponent(messageId)}/translate-report`)
  const o = isRec(raw) ? raw : {}
  const reason = pick(o, 'reason')
  const sentAt = pick(o, 'sent_at')
  return { sent: pick(o, 'sent') === true, reason: typeof reason === 'string' ? reason : null, sentAt: typeof sentAt === 'string' ? sentAt : null }
}

export interface TranslatedPendingQuestions {
  questions: { id: string; header: string; question: string; options: { id: string; label: string; description: string }[] }[]
  model: string
  cached: boolean
  costUsd: number | null
}

export async function translatePendingQuestions(
  questions: { header: string; question: string; options: { label: string; description: string }[] }[],
): Promise<TranslatedPendingQuestions> {
  const raw = await transport.request('POST', '/pending-questions/translate', { questions })
  const o = isRec(raw) ? raw : {}
  const cost = pick(o, 'cost_usd')
  const qs = Array.isArray(pick(o, 'questions')) ? (pick(o, 'questions') as unknown[]) : []
  return {
    questions: qs.filter(isRec).map((q) => ({
      id: str(pick(q, 'id')),
      header: str(pick(q, 'header')),
      question: str(pick(q, 'question')),
      options: (Array.isArray(pick(q, 'options')) ? (pick(q, 'options') as unknown[]) : []).filter(isRec).map((op) => ({
        id: str(pick(op, 'id')),
        label: str(pick(op, 'label')),
        description: str(pick(op, 'description')),
      })),
    })),
    model: str(pick(o, 'model')),
    cached: pick(o, 'cached') === true,
    costUsd: typeof cost === 'number' ? cost : null,
  }
}

export async function fetchUpdateRestart(): Promise<string[]> {
  const raw = await transport.request('GET', '/bots/update-restart')
  return arr(pick(isRec(raw) ? raw : {}, 'bot_ids')).filter((v): v is string => typeof v === 'string')
}

export async function scheduleUpdateRestart(botIds: string[]): Promise<string[]> {
  const raw = await transport.request('POST', '/bots/update-restart', { bot_ids: botIds })
  return arr(pick(isRec(raw) ? raw : {}, 'bot_ids')).filter((v): v is string => typeof v === 'string')
}

export async function cancelUpdateRestart(botIds: string[]): Promise<string[]> {
  const raw = await transport.request('DELETE', '/bots/update-restart', { bot_ids: botIds })
  return arr(pick(isRec(raw) ? raw : {}, 'bot_ids')).filter((v): v is string => typeof v === 'string')
}

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
    restart_status: optStr(pick(o, 'restart_status')) ?? undefined,
  }
}

export async function startCliUpdate(host: string, kind: 'codex' | 'claude', targetVersion: string): Promise<{ update_id: string }> {
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(host)}/cli-update`, { kind, target_version: targetVersion })
  return { update_id: str(pick(isRec(raw) ? raw : {}, 'update_id')) }
}

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

export async function abortBot(botId: string): Promise<{ aborted: string[]; keys_sent: boolean }> {
  const r = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/abort`)
  const o = isRec(r) ? r : {}
  return {
    aborted: Array.isArray(o.aborted) ? o.aborted.map((x) => String(x)) : [],
    keys_sent: o.keys_sent === true,
  }
}

export async function loginBot(botId: string): Promise<{ command: string; kind: string }> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/login`)
  const o = isRec(raw) ? raw : {}
  return { command: str(pick(o, 'command'), '/login'), kind: str(pick(o, 'kind')) }
}

export async function compactBot(botId: string): Promise<{ command: string; kind: string }> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/compact`)
  const o = isRec(raw) ? raw : {}
  return { command: str(pick(o, 'command'), '/compact'), kind: str(pick(o, 'kind')) }
}

export async function setKeepWarmSkip(botId: string, skip: boolean): Promise<{ keep_warm_skip: boolean }> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/keep-warm/skip`, { skip })
  const o = isRec(raw) ? raw : {}
  return { keep_warm_skip: pick(o, 'keep_warm_skip') === true }
}

export async function sendPrompt(
  botId: string,
  text: string,
  clientRequestId: string,
  attachments: string[] = [],
  sendNow = false,
  startIfStopped = false,
  draft?: { action: 'submit' | 'clear'; token: string },
  queueIfBusy = false,
  cacheTtl?: '5m' | '1h',
): Promise<PromptResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/prompt`, {
    ...(draft?.action === 'submit' ? {} : { text }),
    client_request_id: clientRequestId,
    ...(attachments.length ? { attachments } : {}),
    ...(sendNow ? { send_now: true } : {}),
    ...(startIfStopped ? { start_if_stopped: true } : {}),
    ...(queueIfBusy ? { queue_if_busy: true } : {}),
    ...(cacheTtl ? { cache_ttl: cacheTtl } : {}),
    ...(draft ? { [draft.action === 'submit' ? 'submit_draft' : 'clear_draft']: true, expect_draft_token: draft.token } : {}),
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

export async function acceptSuggestion(botId: string, suggestion: string, expectRunId: string | null, clientRequestId: string): Promise<PromptResult> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/suggestion/accept`, {
    suggestion,
    client_request_id: clientRequestId,
    ...(expectRunId ? { expect_run_id: expectRunId } : {}),
  })
  const o = isRec(raw) ? raw : {}
  return {
    turn_id: str(pick(o, 'turn_id')),
    message_id: str(pick(o, 'message_id')) || null,
    delivery: str(pick(o, 'delivery'), 'pending') as PromptResult['delivery'],
    send_now: null,
  }
}

export async function sendKeys(botId: string, keys: string[], expectRunId: string | null): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/keys`, {
    keys,
    ...(expectRunId ? { expect_run_id: expectRunId } : {}),
  })
}

export async function sendText(botId: string, text: string, enter: boolean, expectRunId: string | null, record = false): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/text`, {
    text,
    enter,
    ...(expectRunId ? { expect_run_id: expectRunId } : {}),
    ...(record ? { record: true } : {}),
  })
}

export async function sendQueuedNow(botId: string, expectRunId: string | null): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/send-queued-now`, expectRunId ? { expect_run_id: expectRunId } : {})
}

export async function abandonTurn(turnId: string): Promise<void> {
  await transport.request('POST', `/turns/${encodeURIComponent(turnId)}/abandon`)
}

export async function withdrawTurn(turnId: string): Promise<WithdrawTurnResult> {
  const raw = await transport.request('POST', `/turns/${encodeURIComponent(turnId)}/withdraw`)
  const o = isRec(raw) ? raw : {}
  return {
    text: typeof o.text === 'string' ? o.text : '',
    attachments: Array.isArray(o.attachments) ? o.attachments.filter((id): id is string => typeof id === 'string') : [],
  }
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

export function localImageUrl(botId: string, path: string): Promise<string> {
  return transport.blobUrl(`/bots/${encodeURIComponent(botId)}/local-image?path=${encodeURIComponent(path)}`)
}

export async function fetchModels(kind: BotKind, host?: string, identity?: string | null): Promise<ModelsResponse> {
  const q = new URLSearchParams({ kind })
  if (host && host !== 'local') q.set('host', host)
  if (identity) q.set('identity', identity)
  return toModelsResponse(await transport.request('GET', `/models?${q.toString()}`))
}

export async function fetchQuota(): Promise<QuotaMap> {
  return toQuota(await transport.request('GET', '/quota'))
}

export async function fetchDrafts(): Promise<{ key: string; text: string; rev: number }[]> {
  const raw = await transport.request('GET', '/drafts')
  return arr(isRec(raw) ? raw.drafts : [])
    .filter(isRec)
    .map((d) => ({ key: str(pick(d, 'key')), text: str(pick(d, 'text')), rev: num(pick(d, 'rev'), 0) }))
    .filter((d) => d.key && d.text && d.rev > 0)
}

export async function putDraft(key: string, text: string, clientId: string): Promise<{ rev: number }> {
  const raw = await transport.request('PUT', `/drafts/${encodeURIComponent(key)}`, { text, client_id: clientId })
  return { rev: num(isRec(raw) ? pick(raw, 'rev') : 0, 0) }
}

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

export async function restoreBot(botId: string): Promise<void> {
  await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restore`)
}

export async function fetchMem(): Promise<MemSnapshot> {
  return toMemSnapshot(await transport.request('GET', '/mem'))
}

export async function fetchMemPane(host: string, paneId: string, socket: string | null, lines = 40): Promise<TerminalSnapshot> {
  const sock = socket ? `&socket=${encodeURIComponent(socket)}` : ''
  const raw = await transport.request(
    'GET',
    `/mem/processes/pane?host=${encodeURIComponent(host)}&pane_id=${encodeURIComponent(paneId)}${sock}&lines=${lines}`,
  )
  return toTerminal(raw, 'visible')
}

export async function fetchMemProcesses(host: string, signal?: AbortSignal): Promise<MemProcesses> {
  return toMemProcesses(await transport.request('GET', `/mem/processes?host=${encodeURIComponent(host)}`, undefined, signal))
}

export async function killMemProcess(host: string, pid: number, signal: 'TERM' | 'KILL' = 'TERM'): Promise<void> {
  await transport.request('POST', '/mem/processes/kill', { host, pid, signal })
}

export async function refreshTools(host: string): Promise<{ tools: ToolMap; identity_status: IdentityStatusMap }> {
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/tools/refresh`)
  const rec = isRec(raw) ? raw : {}
  return { tools: toToolMap(rec.tools), identity_status: toIdentityStatusMap(rec.identities) }
}

function toRemoteCargoSettings(raw: unknown): RemoteCargoSettings {
  if (!isRec(raw)) throw new ApiError(404, {}, t('這顆 daemon 還沒有 /api/build/remote（回的是網頁不是 JSON），二進位比前端舊；要 cargo build --release 後重啟 daemon。'))
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
  const state = o.state === 'done' || o.state === 'pending' || o.state === 'failed' ? o.state : 'none'
  return {
    state,
    target_bot_name: str(o.target_bot_name),
    asked_at: str(o.asked_at),
    answered_at: str(o.answered_at),
    result: str(o.result),
    source: o.source === 'api' ? 'api' : '',
    model: str(o.model),
    error: str(o.error),
  }
}

export async function fetchUpdateReview(input: {
  kind: string
  host?: string
  from?: string | null
  to?: string | null
}): Promise<{ version: string; review: UpdateReview; auto?: boolean }> {
  const q = new URLSearchParams({ kind: input.kind })
  if (input.host) q.set('host', input.host)
  if (input.from) q.set('from', input.from)
  if (input.to) q.set('to', input.to)
  const raw = await transport.request('GET', `/claude-update/review?${q}`)
  const o = isRec(raw) ? raw : {}
  return { version: str(o.version), review: toReview(o.review), auto: o.auto === true }
}

export async function requestUpdateReview(input: { kind: string; host?: string; from?: string | null; to?: string | null; deep?: boolean; auto?: boolean }): Promise<{
  version: string
  target_bot_name: string
  source_name: string
  duplicate: boolean
}> {
  const raw = await transport.request('POST', '/claude-update/review', {
    kind: input.kind,
    host: input.host,
    from: input.from ?? undefined,
    to: input.to ?? undefined,
    deep: input.deep ? true : undefined,
    auto: input.auto ? true : undefined,
  })
  const o = isRec(raw) ? raw : {}
  const review = toReview(o.review)
  const source_name = review.source === 'api'
    ? (review.model ? t('Claude API（{model}）', { model: review.model }) : 'Claude API')
    : str(o.target_bot_name)
  return { version: str(o.version), target_bot_name: str(o.target_bot_name), source_name, duplicate: o.duplicate === true }
}

export async function redeemResetCredit(input: { host: string; identity: string | null }): Promise<{ outcome: string; resumed: string[] }> {
  const raw = await transport.request('POST', '/quota/reset-credit/use', {
    host: input.host,
    identity: input.identity ?? undefined,
  })
  const o = isRec(raw) ? raw : {}
  return { outcome: str(o.outcome), resumed: arr(o.resumed).map((x) => str(x)) }
}

export async function snoozeResetCredit(input: { key: string; hours: 1 | 24 | 72 }): Promise<{ snoozedUntil: string }> {
  const raw = await transport.request('PUT', '/quota/reset-credit/snooze', { key: input.key, hours: input.hours })
  const o = isRec(raw) ? raw : {}
  return { snoozedUntil: str(o.snoozed_until) }
}

export async function fetchReleaseTriage(kind: string): Promise<ReturnType<typeof parseTriageRows>> {
  return parseTriageRows(await transport.request('GET', `/release-triage?kind=${encodeURIComponent(kind)}`))
}

export async function loginIdentity(host: string, identity: string): Promise<HostShell> {
  return identityAuth(host, identity, 'login')
}

export async function logoutIdentity(host: string, identity: string): Promise<HostShell> {
  return identityAuth(host, identity, 'logout')
}

export async function logoutKindDefault(host: string, kind: BotKind): Promise<HostShell | null> {
  const name = host || 'local'
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(name)}/kinds/${encodeURIComponent(kind)}/logout`)
  return kind === 'agy' ? null : toHostShell(raw, name)
}

export async function logoutAgy(host: string): Promise<{ removed: boolean }> {
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/agy/logout`)
  return { removed: isRec(raw) && raw.removed === true }
}

export interface AgyInstallResult {
  host: string
  platform: string
  from: string | null
  to: string
  path: string | null
  already_latest: boolean
}

export async function installAgy(host: string): Promise<AgyInstallResult> {
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/agy/install`)
  const r = isRec(raw) ? raw : {}
  return {
    host: typeof r.host === 'string' ? r.host : host,
    platform: typeof r.platform === 'string' ? r.platform : '',
    from: typeof r.from === 'string' ? r.from : null,
    to: typeof r.to === 'string' ? r.to : '',
    path: typeof r.path === 'string' ? r.path : null,
    already_latest: r.already_latest === true,
  }
}

async function identityAuth(host: string, identity: string, op: 'login' | 'logout'): Promise<HostShell> {
  const name = host || 'local'
  const raw = await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(name)}/identities/${encodeURIComponent(identity)}/${op}`,
  )
  return toHostShell(raw, name)
}

export async function installTool(host: string, kind: BotKind, viaBotId: string): Promise<InstallToolResult> {
  return toInstallResult(
    await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/tools/install`, { kind, via_bot_id: viaBotId }),
  )
}

export async function installToolInShell(host: string, kind: BotKind, install_id: string): Promise<{ pane_id: string; cwd: string }> {
  const name = host || 'local'
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(name)}/tools/install`, { kind, install_id })
  const o = isRec(raw) ? raw : {}
  return { pane_id: str(pick(o, 'pane_id')), cwd: str(pick(o, 'cwd')) }
}

function toGhStatus(raw: unknown, fallbackName: string): GhStatus {
  const o = isRec(raw) ? raw : {}
  const pendingRaw = isRec(o.pending) ? o.pending : null
  return {
    name: str(pick(o, 'name'), fallbackName),
    installed: o.installed === true,
    path: optStr(pick(o, 'path')),
    logged_in: typeof o.logged_in === 'boolean' ? o.logged_in : null,
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

export async function fetchHostBaselinePlan(host: string): Promise<HostBaselinePlan | null> {
  const name = host || 'local'
  const raw = await transport.request('GET', `/hosts/${encodeURIComponent(name)}/baseline/plan`)
  return toHostBaselinePlan(raw)
}

export async function applyHostBaseline(
  host: string,
  input: { plan_rev: string; ids?: string[] },
): Promise<HostBaselineApplyResult | null> {
  const name = host || 'local'
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(name)}/baseline/apply`, input)
  return toHostBaselineApplyResult(raw)
}

export async function fetchHostBaselineBackups(host: string): Promise<HostBaselineBackups | null> {
  const name = host || 'local'
  const raw = await transport.request('GET', `/hosts/${encodeURIComponent(name)}/baseline/backups`)
  return toHostBaselineBackups(raw)
}

export async function restoreHostBaseline(host: string, backup_id: string): Promise<HostBaselineRestoreResult | null> {
  const name = host || 'local'
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(name)}/baseline/restore`, { backup_id })
  return toHostBaselineRestoreResult(raw)
}

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
  format: TerminalFormat = 'text',
): Promise<TerminalSnapshot> {
  const fmt = format === 'ansi' ? '&format=ansi' : ''
  const raw = await transport.request(
    'GET',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/terminal?source=${source}&lines=${lines}${fmt}`,
  )
  const snap = toTerminal(raw, source)
  return format === 'ansi' ? { ...snap, format: 'ansi' } : snap
}

export interface ShellEchoOpt {
  echo?: boolean
}

function echoedTerminal(raw: unknown): TerminalSnapshot | null {
  const o = isRec(raw) && isRec(raw.terminal) ? raw.terminal : null
  return o ? { ...toTerminal(o, 'visible'), format: 'ansi' } : null
}

export async function sendHostShellText(
  host: string,
  paneId: string,
  text: string,
  enter = true,
  opt: ShellEchoOpt = {},
): Promise<TerminalSnapshot | null> {
  const raw = await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/text${opt.echo ? '?echo=ansi' : ''}`,
    { text, enter },
  )
  return opt.echo ? echoedTerminal(raw) : null
}

export interface LoginStatus {
  host: string
  pane_id: string
  identity: string
  url: string | null
  awaiting_code: boolean
  code_sent: boolean
  failure: string | null
  kind: string
  flow: 'code' | 'device'
  device_code: string | null
}

export async function fetchLoginStatus(host: string, paneId: string): Promise<LoginStatus | null> {
  try {
    const raw = await transport.request('GET', `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/login`)
    const o = isRec(raw) ? raw : {}
    return {
      host: str(pick(o, 'host'), host),
      pane_id: str(pick(o, 'pane_id'), paneId),
      identity: str(pick(o, 'identity')),
      url: str(pick(o, 'url')) || null,
      awaiting_code: o.awaiting_code === true,
      code_sent: o.code_sent === true,
      failure: str(pick(o, 'failure')) || null,
      kind: str(pick(o, 'kind')) || 'claude',
      flow: pick(o, 'flow') === 'device' ? 'device' : 'code',
      device_code: str(pick(o, 'device_code')) || null,
    }
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null
    throw e
  }
}

export interface LoginCodeResult {
  outcome: 'failed' | 'finished' | 'pending'
  message: string | null
}

export async function submitLoginCode(host: string, paneId: string, code: string): Promise<LoginCodeResult> {
  const raw = await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/login/code`,
    { code },
  )
  const o = isRec(raw) ? raw : {}
  const outcome = str(pick(o, 'outcome'))
  return {
    outcome: outcome === 'failed' || outcome === 'pending' ? outcome : 'finished',
    message: str(pick(o, 'message')) || null,
  }
}

export async function sendHostShellKeys(
  host: string,
  paneId: string,
  keys: string[],
  opt: ShellEchoOpt = {},
): Promise<TerminalSnapshot | null> {
  const raw = await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/keys${opt.echo ? '?echo=ansi' : ''}`,
    { keys },
  )
  return opt.echo ? echoedTerminal(raw) : null
}

export function openHostShellStream(host: string, paneId: string, handlers: StreamHandlers): (() => void) | null {
  if (!transport.openStream) return null
  return transport.openStream(
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/stream?source=visible&format=ansi`,
    handlers,
  )
}

export async function closeHostShell(host: string, paneId: string, confirm = false): Promise<void> {
  await transport.request(
    'DELETE',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}${confirm ? '?confirm=true' : ''}`,
  )
}

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

export async function gitAction(projectId: string, op: 'commit' | 'push' | 'pull', message?: string): Promise<string> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/git/${op}`, op === 'commit' ? { message } : undefined)
  const o = isRec(raw) ? raw : {}
  return str(pick(o, 'output'), '')
}

export async function setPaneAlias(host: string, paneId: string, alias: string): Promise<void> {
  await transport.request('PUT', `/panes/${encodeURIComponent(paneId)}/alias?host=${encodeURIComponent(host)}`, { alias })
}

export async function retryReleaseTriage(kind: string, version: string): Promise<void> {
  await transport.request('POST', '/release-triage/auto', { kind, versions: [version] })
}

export async function publishReleaseTriage(kind: string, version: string): Promise<void> {
  await transport.request('POST', '/release-triage/publish', { kind, version })
}
