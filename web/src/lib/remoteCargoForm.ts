import type { RemoteCargoInput, RemoteCargoSettings } from '../api/types'
import { portProblem } from './hostForm'

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

export interface RemoteCargoProblem {
  field: 'host' | 'user' | 'port' | 'jobs' | 'root'
  text: string
}

/**
 * 儲存前就能知道 daemon 會拒絕（或悄悄改掉）的值（`PUT /build/remote`）：host／user 只收 `A-Za-z0-9._-`（host 另可有 `:`）且不能 `-` 開頭、
 * 工作目錄只收 `A-Za-z0-9._-/` 且不含 `..`、port 1–65535、cargo jobs 1–64（超過 64 daemon 悄悄截成 64）。
 * 以前打錯的數字在 [`toRemoteCargoInput`] 被收斂成預設（port 變 22、jobs 變 4），使用者以為存的是自己打的。
 */
export function remoteCargoProblems(f: RemoteCargoForm): RemoteCargoProblem[] {
  const out: RemoteCargoProblem[] = []
  const host = f.host.trim()
  const user = f.user.trim()
  const root = f.root.trim()
  if (host && (!/^[A-Za-z0-9._:-]+$/.test(host) || host.startsWith('-'))) {
    out.push({ field: 'host', text: '主機只能有英數與 . _ - :，不能以 - 開頭、不能有空白' })
  }
  if (user && (!/^[A-Za-z0-9._-]+$/.test(user) || user.startsWith('-'))) {
    out.push({ field: 'user', text: 'SSH 帳號只能有英數與 . _ -，不能以 - 開頭' })
  }
  const port = portProblem(f.port)
  if (port) out.push({ field: 'port', text: `SSH ${port}` })
  const jobs = f.jobs.trim()
  if (!/^\d{1,3}$/.test(jobs) || Number(jobs) < 1 || Number(jobs) > 64) {
    out.push({ field: 'jobs', text: '遠端 cargo jobs 要是 1–64 的整數（超過 64 daemon 會截成 64）' })
  }
  if (root && (!/^[A-Za-z0-9._/-]+$/.test(root) || root.includes('..'))) {
    out.push({ field: 'root', text: '遠端工作目錄只能有英數與 . _ - /，不能含 ..' })
  }
  return out
}
