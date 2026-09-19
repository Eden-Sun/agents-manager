import type { RemoteCargoInput, RemoteCargoSettings } from '../api/types'

/** 外部 Cargo 設定表單（`HostsPanel` 的 `RemoteCargoPanel`）；數字欄位是使用者打的字串。 */
export interface RemoteCargoForm {
  enabled: boolean
  host: string
  user: string
  port: string
  root: string
  jobs: string
  password: string
  clearPassword: boolean
}

export const DEFAULT_REMOTE_ROOT = '.cache/agents-manager/remote-cargo'

/** 儲存時送給 daemon 的 body（空白、空值、亂打的數字在這裡收斂成預設）。 */
export function toRemoteCargoInput(f: RemoteCargoForm): RemoteCargoInput {
  return {
    enabled: f.enabled,
    host: f.host.trim(),
    user: f.user.trim(),
    ssh_port: Number(f.port) || 22,
    remote_root: f.root.trim() || DEFAULT_REMOTE_ROOT,
    cargo_jobs: Math.max(1, Number(f.jobs) || 4),
    ...(f.password ? { password: f.password } : f.clearPassword ? { password: '' } : {}),
  }
}

/**
 * 表單裡有沒有還沒存的改動。`POST /api/build/remote/test` 不收 body、測的是**已儲存**的設定，
 * 表單有沒存的改動時直接測就是測到舊主機（還會報「可用」），所以測試前要先存。
 * 拿「按儲存會送出的值」跟已儲存的比：空白、`022` 這種打法不同、存下去一樣的不算。
 */
export function remoteCargoUnsaved(saved: RemoteCargoSettings, f: RemoteCargoForm): boolean {
  const next = toRemoteCargoInput(f)
  return (
    next.password !== undefined ||
    next.enabled !== saved.enabled ||
    next.host !== saved.host ||
    next.user !== saved.user ||
    next.ssh_port !== saved.ssh_port ||
    next.remote_root !== saved.remote_root ||
    next.cargo_jobs !== saved.cargo_jobs
  )
}
