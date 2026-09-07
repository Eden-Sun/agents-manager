/**
 * Domain API. Picks the real daemon transport or the in-memory mock
 * (`VITE_MOCK=1`) and exposes typed calls over it.
 */

import { MockTransport } from './mock'
import { toGroupMessagesPage, toHostShell, toHostShells, toInstallResult, toIssueDetail, toIssues, toMessagesPage, toMemProcesses, toMemSnapshot, toModels, toQuota, toState, toTeamDetail, toTeamEvents, toTerminal, toToolMap, toIdentityStatusMap, num, str, isRec, optStr, pick, arr, toSubmodules } from './normalize'
import { HttpTransport } from './transport'
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
  PatchProjectInput,
  NewTeamInput,
  PatchBotInput,
  PatchBotResult,
  PatchTeamInput,
  PromptResult,
  BotKind,
  TeamBranchDisposal,
  TeamControlAction,
  TeamIssueClosed,
  TeamDetail,
  TeamEvent,
  TeamTaskDecision,
  TerminalSnapshot,
  TerminalSource,
  ToolMap,
  TurnDelivery,
ProjectSubmodule,
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

/** API.md §6: `before` = 目前最舊一則的 id，用來往前翻（issue #25 的「載入更早的訊息」）。 */
export async function fetchMessages(botId: string, limit = 200, before?: string): Promise<MessagesPage> {
  const q = new URLSearchParams({ limit: String(limit) })
  if (before) q.set('before', before)
  const raw = await transport.request('GET', `/bots/${encodeURIComponent(botId)}/messages?${q.toString()}`)
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

/**
 * `PATCH /api/projects/:id` (API.md §3). Renaming is never blocked by a live run: the label
 * only feeds the `agent_name` slug of the *next* start.
 */
export async function patchProject(projectId: string, input: PatchProjectInput): Promise<void> {
  await transport.request('PATCH', `/projects/${encodeURIComponent(projectId)}`, input)
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

/**
 * `POST /api/bots/:id/abort` — **強制**結束目前回合。`interrupt` 是「請 agent 停下來」，
 * `esc` 送不出去就整個失敗、回合仍卡在 in-flight；這支反過來，先保證解鎖，送鍵只是順帶
 * （`keys_sent` 告訴你送成功沒有）。
 */
export async function abortBot(botId: string): Promise<{ aborted: string[]; keys_sent: boolean }> {
  const r = await transport.request('POST', `/bots/${encodeURIComponent(botId)}/abort`)
  const o = isRec(r) ? r : {}
  return {
    aborted: Array.isArray(o.aborted) ? o.aborted.map((x) => String(x)) : [],
    keys_sent: o.keys_sent === true,
  }
}

/**
 * `POST /api/bots/:id/login` — 對這個**正在跑的** bot 的 TUI 送 `/login`，讓它進入
 * 登入 / 切換帳號流程。回傳實際送進去的那一行。
 *
 * 只是把指令送進去而已：agent 接著會停在登入畫面（通常還會開瀏覽器），完成與否得靠
 * `refreshTools` 重新偵測。失敗一律是 `ApiError`，理由在 `body.error` / `body.reason`
 * （`login_unsupported` / `not_running` / `agent_busy` / `turn_in_flight` / `no_pane`）。
 */
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

/**
 * `POST /bots/:id/text` — 把一整段文字打進 bot 的 pane（daemon 端是 `pane.send_text`），
 * `enter` 決定要不要接一個 Enter。多行文字走這裡，不要拆成 `sendKeys` 的鍵名。
 */
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
/**
 * `identity` (claude only) picks whose `settings.json` the "預設" effort hint is read from
 * (SPEC §17.1) — omit it for the default account. An unknown name just falls back to that
 * account, so it is safe to pass through whatever the form currently has selected.
 */
export async function fetchModels(kind: BotKind, host?: string, identity?: string | null): Promise<ModelInfo[]> {
  const q = new URLSearchParams({ kind })
  if (host && host !== 'local') q.set('host', host)
  if (identity) q.set('identity', identity)
  return toModels(await transport.request('GET', `/models?${q.toString()}`))
}

/** `GET /api/quota` — per-kind 5h / 7d usage. */
export async function fetchQuota(): Promise<QuotaMap> {
  return toQuota(await transport.request('GET', '/quota'))
}

/**
 * `GET /api/search/messages?q=` — 哪些 bot 的對話裡出現過這段文字。
 * 舊 daemon 沒有這支 → 丟出來由呼叫端當成「沒有訊息命中」。
 */
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

/**
 * `POST /api/bots/:id/restore` — 把誤刪的 bot 放回來（連同它全部的對話）。
 * 舊 daemon 沒有這支 → false，呼叫端顯示失敗而不是假裝成功。
 */
export async function restoreBot(botId: string): Promise<boolean> {
  try {
    await transport.request('POST', `/bots/${encodeURIComponent(botId)}/restore`)
    return true
  } catch {
    return false
  }
}

/** `GET /api/mem` — herdr 進程樹的常駐記憶體（SPEC §15）。 */
export async function fetchMem(): Promise<MemSnapshot> {
  return toMemSnapshot(await transport.request('GET', '/mem'))
}

/**
 * `GET /api/mem/processes?host=…` — 那個數字是由哪些程序組成的（SPEC §15.2）。
 * 舊 daemon 沒有這支 → 空清單，popover 顯示「這台 daemon 還不會列」而不是壞掉。
 */
/** `GET /api/mem/processes/pane` — 清單裡「自己開的 pane」現在畫面上的字（SPEC §15.2）。 */
export async function fetchMemPane(host: string, paneId: string, socket: string | null, lines = 40): Promise<TerminalSnapshot> {
  const sock = socket ? `&socket=${encodeURIComponent(socket)}` : ''
  const raw = await transport.request(
    'GET',
    `/mem/processes/pane?host=${encodeURIComponent(host)}&pane_id=${encodeURIComponent(paneId)}${sock}&lines=${lines}`,
  )
  return toTerminal(raw, 'visible')
}

export async function fetchMemProcesses(host: string): Promise<MemProcesses> {
  try {
    return toMemProcesses(await transport.request('GET', `/mem/processes?host=${encodeURIComponent(host)}`))
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return { host, sampled_at: '', processes: [] }
    throw e
  }
}

/**
 * `POST /api/mem/processes/kill` — 結束一個 herdr 樹裡的程序（SPEC §15.2）。
 * daemon 會重新取樣再判定，並擋掉 herdr 本身（400）與 bot（409）；錯誤原樣往上丟，
 * 呼叫端把 daemon 的訊息照著顯示，不在前端另外猜一套說法。
 */
export async function killMemProcess(host: string, pid: number, signal: 'TERM' | 'KILL' = 'TERM'): Promise<void> {
  await transport.request('POST', '/mem/processes/kill', { host, pid, signal })
}

/**
 * `POST /api/hosts/:name/tools/refresh` — re-runs CLI + per-identity login detection on that
 * host. Detection otherwise only happens when the host (re)connects, so this is what the user
 * reaches for right after logging an account in.
 */
export async function refreshTools(host: string): Promise<{ tools: ToolMap; identity_status: IdentityStatusMap }> {
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(host || 'local')}/tools/refresh`)
  const rec = isRec(raw) ? raw : {}
  return { tools: toToolMap(rec.tools), identity_status: toIdentityStatusMap(rec.identities) }
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

/** `GET /api/hosts/:name/gh` — 該主機的 GitHub CLI 登入狀態。 */
export async function fetchGhStatus(host: string): Promise<GhStatus> {
  const name = host || 'local'
  return toGhStatus(await transport.request('GET', `/hosts/${encodeURIComponent(name)}/gh`), name)
}

/** `POST /api/hosts/:name/gh/login` — auto / copy / device / switch。 */
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

/** `POST /api/hosts/:name/gh/cancel` — 放棄進行中的裝置碼。 */
export async function cancelGhLogin(host: string): Promise<GhStatus> {
  const name = host || 'local'
  return toGhStatus(await transport.request('POST', `/hosts/${encodeURIComponent(name)}/gh/cancel`), name)
}

// ------------------------------------------------------------------ 主機 shell

/**
 * `POST /api/hosts/:name/shells` — 在某台主機開一個純 shell pane（沒有 agent、沒有 run）。
 *
 * `cwd` 省略時由 daemon 挑（該主機的某個 project，都沒有就 `$HOME`），回傳的一律是**實際**
 * 開起來的目錄。`pane_id` 只在 daemon 這一輪有效：它同時是白名單的 key，daemon 重啟後
 * 舊的 pane 一律不認。
 */
export async function openHostShell(host: string, cwd?: string): Promise<HostShell> {
  const name = host || 'local'
  const raw = await transport.request('POST', `/hosts/${encodeURIComponent(name)}/shells`, cwd ? { cwd } : {})
  return toHostShell(raw, name)
}

/** `GET /api/hosts/:name/shells` — 這台主機上還活著的 shell（daemon 會順手掃掉死掉的）。 */
export async function fetchHostShells(host: string): Promise<HostShell[]> {
  const name = host || 'local'
  return toHostShells(await transport.request('GET', `/hosts/${encodeURIComponent(name)}/shells`), name)
}

/** `GET /api/hosts/:name/shells/:pane_id/terminal` — 形狀同 `GET /bots/:id/terminal`。 */
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

/**
 * `POST /api/hosts/:name/shells/:pane_id/text` — 把一行字打進 shell。
 *
 * `enter` 是 daemon 端獨立的一次按鍵，不是文字裡的 `\n`（對 herdr 來說換行是「貼上」而不是
 * 「按 Enter」）。空字串 + `enter` = 只按 Enter，這在提示符前是真的會用到的動作。
 */
export async function sendHostShellText(host: string, paneId: string, text: string, enter = true): Promise<void> {
  await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/text`,
    { text, enter },
  )
}

/** `POST /api/hosts/:name/shells/:pane_id/keys` — 鍵名原樣送 herdr（同 `usePaneKeys`）。 */
export async function sendHostShellKeys(host: string, paneId: string, keys: string[]): Promise<void> {
  await transport.request(
    'POST',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}/keys`,
    { keys },
  )
}

/** `DELETE /api/hosts/:name/shells/:pane_id` — 結束這個 shell。已經沒了也算成功。 */
export async function closeHostShell(host: string, paneId: string): Promise<void> {
  await transport.request(
    'DELETE',
    `/hosts/${encodeURIComponent(host || 'local')}/shells/${encodeURIComponent(paneId)}`,
  )
}

/**
 * 「這版 daemon 沒有主機 shell」與真正的失敗分開（docs/FRONTEND.md §8），判準同
 * `isPaneMoveUnsupported`：405 / 501 一律當沒實作；404 只在**沒有機器碼**時算——daemon 自己的
 * 404 一定帶 `{error, what}`，那是「主機不見了」或「這個 shell 已經關了」，不是缺功能。
 */
export function isHostShellUnsupported(e: unknown): boolean {
  if (!(e instanceof ApiError)) return false
  if (e.status === 405 || e.status === 501) return true
  if (e.status !== 404) return false
  return !e.body.error && !e.body.what
}

/** `GET /api/projects/:id/issues?state=&limit=&q=` (v4.0; the daemon shells out to `gh`). */
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

/** `GET /api/projects/:id/issues/:number` — with the full body. */
export async function fetchIssue(projectId: string, number: number, repo = ''): Promise<IssueDetail | null> {
  const qs = repo ? `?repo=${encodeURIComponent(repo)}` : ''
  return toIssueDetail(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/issues/${number}${qs}`))
}

/**
 * `GET /api/projects/:id/submodules` — the project's git submodules with their GitHub origins.
 * 舊 daemon 沒有這個端點：裸 404 當成「沒有 submodule」，不是錯誤。
 */
export async function fetchSubmodules(projectId: string): Promise<ProjectSubmodule[]> {
  try {
    return toSubmodules(await transport.request('GET', `/projects/${encodeURIComponent(projectId)}/submodules`))
  } catch (e) {
    if (isTeamsUnsupported(e)) return []
    throw e
  }
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

/** `POST /api/teams/:id/issues`（SPEC-team §2.3）——把 issue 追加到執行中 team 的佇列。 */
export async function addTeamIssues(teamId: string, issueNumbers: number[]): Promise<void> {
  await transport.request('POST', `/teams/${encodeURIComponent(teamId)}/issues`, { issue_numbers: issueNumbers })
}

/** `DELETE /api/teams/:id/issues/:issue_id` — 只能移除還沒開始的（`queued`）。 */
export async function removeTeamIssue(teamId: string, issueId: string): Promise<void> {
  await transport.request(
    'DELETE',
    `/teams/${encodeURIComponent(teamId)}/issues/${encodeURIComponent(issueId)}`,
  )
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

export interface CloseTeamIssueInput {
  /** 省略 = daemon 寫預設的完成留言；空字串 = 不留言。 */
  comment?: string
  /**
   * SPEC-team §2.5.4：要關的是佇列裡哪一筆。省略 = `teams.issue_number` 對到的那一筆
   * （reopen 之前唯一存在的行為）。reopen 之後鏡像已經換成新 issue，要回頭關上一個就得帶它。
   */
  issue_id?: string
}

/**
 * `POST /api/teams/:id/close-issue`（SPEC-team §10.7）→ `{number, url, state, already_closed}`。
 *
 * 只有 `state === 'done'` 的那一筆 issue 能關，而且**只由使用者按下按鈕觸發**——daemon 不會
 * 自己關 issue。預設留言是 PM 總結 + 整合分支 + 已合併的 commit（沒有 PR 時會註明「分支還沒
 * 合併進 base」）。別人已經先關掉的 issue 回 `already_closed: true`，不是錯誤。
 */
export async function closeTeamIssue(teamId: string, input?: CloseTeamIssueInput): Promise<TeamIssueClosed> {
  const raw = await transport.request('POST', `/teams/${encodeURIComponent(teamId)}/close-issue`, {
    ...(input?.comment === undefined ? {} : { comment: input.comment }),
    ...(input?.issue_id === undefined ? {} : { issue_id: input.issue_id }),
  })
  const o = isRec(raw) ? raw : {}
  return {
    number: num(pick(o, 'number'), 0),
    url: str(pick(o, 'url')),
    already_closed: o.already_closed === true,
  }
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

// -------------------------------------------------------- 快速 git（chat 標題列的 chip，2026-09-08）

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

/** `GET /api/projects/:id/git`。舊 daemon 沒這支端點（404）→ `{git:false}`，chip 靜默消失。 */
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

/** `POST …/git/commit`（`git add -A && git commit`）、`…/git/push`、`…/git/pull --rebase --no-autostash`。回 git 的輸出。 */
export async function gitAction(projectId: string, op: 'commit' | 'push' | 'pull', message?: string): Promise<string> {
  const raw = await transport.request('POST', `/projects/${encodeURIComponent(projectId)}/git/${op}`, op === 'commit' ? { message } : undefined)
  const o = isRec(raw) ? raw : {}
  return str(pick(o, 'output'), '')
}
