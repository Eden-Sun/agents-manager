import * as api from '../api'
import { ApiError } from '../api/types'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'
import { openFreshShellAndType } from './cliLogin'
import { cliLoginCommand } from './quotaLogin'
import { hostLabel } from '../components/toolsHelpers'
import { t } from './i18n'
import { plainErrMsg } from './errText'

function errText(e: unknown): string {
  if (e instanceof ApiError && typeof e.body.message === 'string') return e.body.message
  return plainErrMsg(e)
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
    if (!ownsInstall(key, installId)) return true
    useStore.getState().viewHostShell({ host: host || 'local', pane_id: r.pane_id, tab_id: '', workspace_id: '', cwd: r.cwd, created_at: '' })
    const targetHost = hostLabel(!host || host === 'local' ? 'local' : host)
    useStore.getState().notify('info', t('已在 {host} 開 shell 安裝 {kind}，裝完會自動重新偵測', { host: targetHost, kind }))
    setTimeout(() => releaseInstall(key, installId), 16 * 60_000)
    return true
  } catch (e) {
    useStore.getState().notify('error', t('安裝 {kind} 失敗：{reason}', { kind, reason: errText(e) }))
    releaseInstall(key, installId)
    return false
  }
}

export async function loginKindOnHost(host: string, kind: BotKind, identity: string | null): Promise<boolean> {
  const s = useStore.getState()
  if (identity) return s.loginIdentity(host, identity)
  if (!(await s.confirmCodexLoginReplace(host, kind, null))) return false
  const { opened } = await openFreshShellAndType(host, cliLoginCommand(kind, {}))
  return opened
}

export const installBusyKey = busyKey
