/**
 * 選單那排 shell／服務 pane 怎麼稱呼（SPEC §6.5e）。拆出來是因為這是唯一值得釘測試的部分：
 * 一顆 pane 可能什麼都沒有（沒用途、沒前景程式），標題不能因此變成空字串——那樣就點不到了。
 */
import type { ProjectPane } from '../api'
import { t } from './i18n'

/** 別名優先；預設指令加 listen port。永遠回得出東西。 */
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

/** 滑過去才看得到的那一行：在哪、有沒有開 port、是不是使用者自己開的。 */
export function paneHint(p: ProjectPane): string {
  const bits = [p.cwd || p.pane_id]
  if (p.listen_ports.length > 0) bits.push(t('listen {ports}', { ports: p.listen_ports.join(t('、')) }))
  if (p.owned_by === 'user') bits.push(t('你自己開的'))
  return bits.join(' · ')
}
