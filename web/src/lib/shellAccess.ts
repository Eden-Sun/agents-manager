import { ApiError } from '../api/types'
import type { ProjectPane } from '../api'
import { t } from './i18n'

export function paneReadOnly(p: Pick<ProjectPane, 'read_only' | 'listen_ports'>): boolean {
  return p.read_only ?? p.listen_ports.length > 0
}

export function shellForbidden(e: unknown): string | null {
  if (!(e instanceof ApiError) || e.status !== 403) return null
  const kind = e.body.error
  if (kind !== 'read_only_pane' && kind !== 'agent_pane') return null
  if (typeof e.body.message === 'string' && e.body.message.trim()) return e.body.message
  return kind === 'agent_pane' ? t('這顆 pane 正在跑 agent，請走 bot 對話') : t('這顆 pane 只能看不能打字')
}

export function shellStateUnknown(e: unknown): string | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'pane_state_unknown') return null
  const msg = typeof e.body.message === 'string' ? e.body.message.trim() : ''
  return msg || t('無法確認這顆 pane 現在的狀態，這次沒有送出，請稍後再試')
}

export function keySyncActive(remembered: boolean, readOnly: boolean): boolean {
  return remembered && !readOnly
}

export function initialKeySync(target: string, opts: { phone: boolean; off: ReadonlySet<string>; on: ReadonlySet<string> }): boolean {
  if (opts.off.has(target)) return false
  if (opts.on.has(target)) return true
  return !opts.phone
}
