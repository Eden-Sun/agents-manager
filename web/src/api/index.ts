import { MockTransport } from './mock'
import { toMission, toMissionDetail, toMissionEvent, toMissions, toGroupMessagesPage, toHostShell, toHostShells, toInstallResult, toIssueDetail, toIssues, toMessagesPage, toMemProcesses, toMemSnapshot, toModels, toQuota, toState, toTerminal, toToolMap, toIdentityStatusMap, num, str, isRec, optStr, pick, arr, toSubmodules } from './normalize'
import { HttpTransport, pairWithCode } from './transport'
import { ApiError } from './types'
import type { SocketHandlers, Transport } from './transport'
import type {
  MemProcesses,
  MemSnapshot,
  MessageHit,
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
  PairCode,
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

/**
 * SPEC §7.1a：在本機產一個一次性配對碼給另一台裝置輸入。
 * 只有 loopback 產得出來（否則 403 `loopback_only`）——把權限交出去的人得站在這台機器前面。
 */
export async function issuePairCode(): Promise<PairCode> {
  const raw = await transport.request('POST', '/session/pair-code')
  const o = isRec(raw) ? raw : {}
  return {
    code: str(pick(o, 'code')),
    expires_in_secs: num(pick(o, 'expires_in_secs'), 300),
    expires_at: optStr(pick(o, 'expires_at')),
  }
}

/** 拿配對碼換 token 並存在這台裝置上。失敗丟 `ApiError`，文案交給 `lib/pairing`。 */
export function pairDevice(code: string): Promise<void> {
  return pairWithCode(transport, code)
}

/** token 失效又需要重新配對時回呼（store 據此把畫面換回配對畫面）。 */
export function onPairingRequired(cb: (() => void) | null): void {
  transport.setPairingListener(cb)
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

export async function createBot(projectId: string, input: NewBotInput): Promise<{ id: string; name: string }> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/bots`, input)
  const o = isRec(raw) ? raw : {}
  return { id: str(pick(o, 'bot_id')), name: str(pick(o, 'name')) }
}

/** API.md v3.3. `null` `model`/`identity` clears; renaming with an active Run → 409. */
export async function patchBot(botId: string, input: PatchBotInput): Promise<PatchBotResult> {
  const raw = await transport.request('PATCH', `/bots/${encodeURIComponent(botId)}`, input)
  const o = isRec(raw) ? raw : {}
  return { needs_restart: o.needs_restart === true || o.restart_required === true }
}

/** 側欄排序（API.md §5.4），存 config.toml 讓各裝置共用同一份。 */
export async function saveOrder(input: { projects?: string[]; bots?: Record<string, string[]> }): Promise<void> {
  await transport.request('POST', '/order', input)
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

export async function restartBot(botId: string): Promise<string> {
  const raw = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restart`)
  return isRec(raw) ? str(pick(raw, 'run_id')) : ''
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

/** 任何失敗 → false，呼叫端顯示失敗而不是假裝成功。 */
export async function restoreBot(botId: string): Promise<boolean> {
  try {
    await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restore`)
    return true
  } catch {
    return false
  }
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
export async function closeHostShell(host: string, paneId: string): Promise<void> {
  await transport.request(
    'DELETE',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}`,
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

export async function fetchMissions(
  projectId: string,
  status: 'all' | 'open' | 'done' | 'cancelled' = 'all',
  limit = 50,
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
