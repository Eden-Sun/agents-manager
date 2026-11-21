import { isRec, pick, str } from '../api/normalize'
import type { BotKind } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { useStore } from '../store/store'
import { installBusyKey, releaseInstall } from './onboardingActions'
import { hostLabel } from '../components/toolsHelpers'
import { t } from './i18n'

export interface ToolInstallDone {
  host: string
  install_id: string
  outcome: string
  kind: BotKind
  installed: boolean
  pane_id: string
  pane_kept: boolean
  timed_out: boolean
}

export function toToolInstallDone(data: unknown): ToolInstallDone | null {
  if (!isRec(data)) return null
  const kind = str(pick(data, 'kind')) as BotKind
  if (!BOT_KINDS.includes(kind) || !str(data.install_id)) return null
  return {
    host: str(pick(data, 'host'), 'local') || 'local',
    kind,
    install_id: str(data.install_id),
    outcome: str(data.outcome),
    installed: data.installed === true,
    pane_id: str(pick(data, 'pane_id')),
    pane_kept: data.pane_kept === true,
    timed_out: data.timed_out === true,
  }
}

export function toolInstallDoneText(ev: ToolInstallDone): { level: 'info' | 'error'; text: string; openPane: boolean } {
  const where = hostLabel(ev.host === 'local' ? 'local' : ev.host)
  if (ev.outcome === 'superseded') return { level: 'error', text: t('{where} 的安裝追蹤已失聯（主機連線或偵測已換代），請重新偵測並確認原安裝程序', { where }), openPane: false }
  if (ev.timed_out) return { level: 'error', text: t('{where} 安裝 {kind} 等了 15 分鐘仍未確認結束（逾時）；請檢查保留的安裝終端', { where, kind: ev.kind }), openPane: true }
  if (ev.installed) return { level: 'info', text: t('{where} 已安裝 {kind}', { where, kind: ev.kind }), openPane: false }
  const why = t('安裝結束後仍偵測不到')
  if (ev.pane_kept && ev.pane_id) return { level: 'error', text: t('{where} 沒裝成 {kind}：{why}；安裝終端留著，錯誤輸出在裡面', { where, kind: ev.kind, why }), openPane: true }
  return { level: 'error', text: t('{where} 沒裝成 {kind}：{why}；安裝終端已關閉，請再按一次「安裝」', { where, kind: ev.kind, why }), openPane: false }
}

export async function handleToolInstallDone(data: unknown): Promise<void> {
  const ev = toToolInstallDone(data)
  if (!ev) return
  const key = installBusyKey(ev.host, ev.kind)
  if (!releaseInstall(key, ev.install_id)) return
  const r = toolInstallDoneText(ev)
  useStore.getState().notify(r.level, r.text)
  if (ev.outcome === 'superseded' || ev.timed_out) return
  if (ev.installed) {
    void useStore.getState().refreshTools(ev.host)
    const view = useStore.getState().shellView
    if (view && view.host === ev.host && view.paneId === ev.pane_id) useStore.getState().closeShellView()
    return
  }
}
