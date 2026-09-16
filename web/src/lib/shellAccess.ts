/**
 * shell 面板「能不能打字」的判斷（SPEC §6.5e）。
 *
 * 以前前端用 `kind === 'service'` 決定唯讀，而且只在點下去那一刻決定；daemon 的掃描一看到前景程式
 * （vim、less、sudo、python…）就把 pane 分成 service，之後每一下按鍵都 403，面板卻還開著輸入框，
 * 使用者卡在 vim 裡出不來（第二輪 review H2）。daemon 改成「有 listen port 才唯讀」並在 pane 列給
 * `read_only`；前端照它判斷，舊 daemon 沒這欄時退回看 port。
 */
import { ApiError } from '../api/types'
import type { ProjectPane } from '../api'

export function paneReadOnly(p: Pick<ProjectPane, 'read_only' | 'listen_ports'>): boolean {
  return p.read_only ?? p.listen_ports.length > 0
}

/**
 * daemon 回 403 說「這顆不給打字」時的說明文字；不是這種錯誤回 `null`。
 * `read_only_pane`＝有 port 的服務 pane；`agent_pane`＝裡面正在跑 agent，要走 bot 對話。
 */
export function shellForbidden(e: unknown): string | null {
  if (!(e instanceof ApiError) || e.status !== 403) return null
  const kind = e.body.error
  if (kind !== 'read_only_pane' && kind !== 'agent_pane') return null
  if (typeof e.body.message === 'string' && e.body.message.trim()) return e.body.message
  return kind === 'agent_pane' ? '這顆 pane 正在跑 agent，請走 bot 對話' : '這顆 pane 只能看不能打字'
}

/**
 * daemon 確認不了 pane 現在的狀態（409 `pane_state_unknown`，AGM 驗收 9b54b6e：讀不到就不打）：
 * 這次沒送出去、但**不是唯讀**——稍後再試就好，所以不鎖面板，只把 daemon 的說明顯示出來。
 */
export function shellStateUnknown(e: unknown): string | null {
  if (!(e instanceof ApiError) || e.status !== 409 || e.body.reason !== 'pane_state_unknown') return null
  const msg = typeof e.body.message === 'string' ? e.body.message.trim() : ''
  return msg || '無法確認這顆 pane 現在的狀態，這次沒有送出，請稍後再試'
}

/** 鍵盤同步實際上開著沒：唯讀時一律不算，localStorage 記著「開」也一樣（第二輪 review L1）。 */
export function keySyncActive(remembered: boolean, readOnly: boolean): boolean {
  return remembered && !readOnly
}
