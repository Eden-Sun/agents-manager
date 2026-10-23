import * as api from '../api'
import { isRec, pick, str } from '../api/normalize'
import type { BotKind } from '../api/types'
import { BOT_KINDS } from '../api/types'
import { useStore } from '../store/store'
import { installBusyKey } from './onboardingActions'

/** WS `tool_install_done`（API.md §8／§12.7）：不帶 `via_bot_id` 的直接安裝收尾。借 bot 安裝不推這個事件。 */
export interface ToolInstallDone {
  host: string
  kind: BotKind
  /** 安裝後重新偵測的結果。 */
  installed: boolean
  pane_id: string
  /** `false` ＝使用者自己關了 pane；`installed` 為 `false` 且為 `true` 時錯誤輸出還在那顆 pane。 */
  pane_kept: boolean
  /** 等滿 15 分鐘。 */
  timed_out: boolean
}

export function toToolInstallDone(data: unknown): ToolInstallDone | null {
  if (!isRec(data)) return null
  const kind = str(pick(data, 'kind')) as BotKind
  if (!BOT_KINDS.includes(kind)) return null
  return {
    host: str(pick(data, 'host'), 'local') || 'local',
    kind,
    installed: data.installed === true,
    pane_id: str(pick(data, 'pane_id')),
    pane_kept: data.pane_kept === true,
    timed_out: data.timed_out === true,
  }
}

/** 給使用者看的結果（純函式，方便測）。 */
export function toolInstallDoneText(ev: ToolInstallDone): { level: 'info' | 'error'; text: string; openPane: boolean } {
  const where = ev.host === 'local' ? '本機' : ev.host
  if (ev.installed) return { level: 'info', text: `${where} 已安裝 ${ev.kind}`, openPane: false }
  const why = ev.timed_out ? `等了 15 分鐘安裝還沒結束（逾時）` : '安裝結束後仍偵測不到'
  if (ev.pane_kept && ev.pane_id) return { level: 'error', text: `${where} 沒裝成 ${ev.kind}：${why}；安裝終端留著，錯誤輸出在裡面`, openPane: true }
  return { level: 'error', text: `${where} 沒裝成 ${ev.kind}：${why}；安裝終端已關閉，請再按一次「安裝」`, openPane: false }
}

/** 收尾：放開那一列的「安裝中…」、講結果；沒裝成且 pane 還在就切到它讓使用者看錯誤。 */
export async function handleToolInstallDone(data: unknown): Promise<void> {
  const ev = toToolInstallDone(data)
  if (!ev) return
  const key = installBusyKey(ev.host, ev.kind)
  useStore.setState((st) => {
    if (!st.busy[key]) return st
    const busy = { ...st.busy }
    delete busy[key]
    return { busy }
  })
  const r = toolInstallDoneText(ev)
  useStore.getState().notify(r.level, r.text)
  if (ev.installed) {
    // host_changed 通常已經帶了新 tools；再問一次，事件順序顛倒時那一列也會翻。
    void useStore.getState().refreshTools(ev.host)
    // 裝成功 daemon 會關掉安裝 pane：使用者正看著它的話一起離開。
    const view = useStore.getState().shellView
    if (view && view.host === ev.host && view.paneId === ev.pane_id) useStore.getState().closeShellView()
    return
  }
  if (!r.openPane) return
  let cwd = ''
  try {
    cwd = (await api.fetchHostShells(ev.host)).find((s) => s.pane_id === ev.pane_id)?.cwd ?? ''
  } catch {
    /* 讀不到 cwd 只是標題少一段，pane 還是要開 */
  }
  useStore.getState().viewHostShell({ host: ev.host, pane_id: ev.pane_id, tab_id: '', workspace_id: '', cwd, created_at: '' })
}
