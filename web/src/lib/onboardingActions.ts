import * as api from '../api'
import { ApiError } from '../api/types'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'
import { openFreshShellAndType } from './cliLogin'
import { cliLoginCommand } from './quotaLogin'
import { hostLabel } from '../components/toolsHelpers'
import { t } from './i18n'

function errText(e: unknown): string {
  if (e instanceof ApiError && typeof e.body.message === 'string') return e.body.message
  return e instanceof Error ? e.message : String(e)
}

const installs = new Map<string, string>()
export function ownsInstall(key: string, id: string): boolean { return installs.get(key) === id }
export function releaseInstall(key: string, id: string): boolean {
  if (!ownsInstall(key, id)) return false
  installs.delete(key)
  useStore.setState((st) => {
    const busy = { ...st.busy }
    delete busy[key]
    return { busy }
  })
  return true
}

const busyKey = (host: string, kind: BotKind) => `install:${host || 'local'}:${kind}`

/**
 * 沒有 Bot 可借時安裝 CLI（issue #1931）：claude／codex／grok 由 daemon 在該主機開臨時 shell pane 跑官方安裝指令，
 * 畫面切到那個 pane 看進度；結束後 daemon 重新偵測並推 `host_changed`，清單自己翻成「已安裝」。agy 沿用 `installAgy`。
 */
export async function installKindOnHost(host: string, kind: BotKind): Promise<boolean> {
  const s = useStore.getState()
  if (kind === 'agy') return s.installAgy(host)
  const key = busyKey(host, kind)
  if (s.busy[key]) return false
  const installId = globalThis.crypto?.randomUUID?.() ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`
  installs.set(key, installId)
  useStore.setState((st) => ({ busy: { ...st.busy, [key]: true } }))
  try {
    const r = await api.installToolInShell(host, kind, installId)
    // A completion can arrive before the HTTP response; do not reopen its pane.
    if (!ownsInstall(key, installId)) return true
    useStore.getState().viewHostShell({ host: host || 'local', pane_id: r.pane_id, tab_id: '', workspace_id: '', cwd: r.cwd, created_at: '' })
    const targetHost = hostLabel(!host || host === 'local' ? 'local' : host)
    useStore.getState().notify('info', t('已在 {host} 開 shell 安裝 {kind}，裝完會自動重新偵測', { host: targetHost, kind }))
    // 鈕維持「安裝中…」到 WS `tool_install_done`（`handleToolInstallDone` 放開）；事件漏了的保險：daemon 最久等 15 分鐘。
    setTimeout(() => releaseInstall(key, installId), 16 * 60_000)
    return true
  } catch (e) {
    useStore.getState().notify('error', t('安裝 {kind} 失敗：{reason}', { kind, reason: errText(e) }))
    releaseInstall(key, installId)
    return false
  }
}

/**
 * 登入一個 kind／身份（沿用既有流程，不另開一套）：命名身份走 daemon 的 `identities/{id}/login`（claude 有登入協助面板）；
 * 預設帳號開新 shell 打 `<cli> login`（agy 是打 `agy` 由 TUI 引導）。回「登入終端開起來了」。
 */
export async function loginKindOnHost(host: string, kind: BotKind, identity: string | null): Promise<boolean> {
  const s = useStore.getState()
  if (identity) return s.loginIdentity(host, identity)
  const { opened } = await openFreshShellAndType(host, cliLoginCommand(kind, {}))
  return opened
}

export const installBusyKey = busyKey
