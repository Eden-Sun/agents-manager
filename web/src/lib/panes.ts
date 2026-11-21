import type { ProjectPane } from '../api'
import { t } from './i18n'

export function paneLabel(p: ProjectPane): string {
  if (p.alias?.trim()) return p.alias.trim()
  const label = paneCommand(p)
  return p.listen_ports.length ? `${label} :${p.listen_ports.join(', :')}` : label
}

export function paneCommand(p: ProjectPane): string {
  if (p.foreground?.trim()) {
    const exe = p.foreground.trim().split(/\s+/)[0].split('/').pop()
    if (exe) return exe
  }
  if (p.purpose?.trim()) return p.purpose.trim()
  const dir = (p.cwd ?? '').replace(/\/+$/, '').split('/').pop()
  return dir || p.pane_id
}

export function paneHint(p: ProjectPane): string {
  const bits = [p.cwd || p.pane_id]
  if (p.listen_ports.length > 0) bits.push(t('listen {ports}', { ports: p.listen_ports.join(t('、')) }))
  if (p.owned_by === 'user') bits.push(t('你自己開的'))
  return bits.join(' · ')
}
