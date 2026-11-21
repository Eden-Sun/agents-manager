import type { RemoteCargoInput, RemoteCargoSettings } from '../api/types'
import { t } from './i18n'
import { portProblem } from './hostForm'

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

export function remoteCargoProblems(f: RemoteCargoForm): RemoteCargoProblem[] {
  const out: RemoteCargoProblem[] = []
  const host = f.host.trim()
  const user = f.user.trim()
  const root = f.root.trim()
  if (host && (!/^[A-Za-z0-9._:-]+$/.test(host) || host.startsWith('-'))) {
    out.push({ field: 'host', text: t('主機只能有英數與 . _ - :，不能以 - 開頭、不能有空白') })
  }
  if (user && (!/^[A-Za-z0-9._-]+$/.test(user) || user.startsWith('-'))) {
    out.push({ field: 'user', text: t('SSH 帳號只能有英數與 . _ -，不能以 - 開頭') })
  }
  const port = portProblem(f.port)
  if (port) out.push({ field: 'port', text: `SSH ${port}` })
  const jobs = f.jobs.trim()
  if (!/^\d{1,3}$/.test(jobs) || Number(jobs) < 1 || Number(jobs) > 64) {
    out.push({ field: 'jobs', text: t('遠端 cargo jobs 要是 1–64 的整數（超過 64 daemon 會截成 64）') })
  }
  if (root && (!/^[A-Za-z0-9._/-]+$/.test(root) || root.includes('..'))) {
    out.push({ field: 'root', text: t('遠端工作目錄只能有英數與 . _ - /，不能含 ..') })
  }
  return out
}
