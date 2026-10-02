import type { GroupChatResult } from '../api/types'

/** 一顆都沒送到（全被跳過）＝ false。舊 daemon 沒有 `delivered` 欄，退回看 `sent`。 */
export function groupSendDelivered(res: Pick<GroupChatResult, 'delivered' | 'sent'>): boolean {
  return res.delivered ?? res.sent.length > 0
}

/** 跳過的收件者為什麼沒送到，給 toast 用（daemon 的 `detail` 是英文原文；機器碼 §13.3 翻成人話）。 */
export function groupSkipText(x: { reason: string; detail: string }): string {
  switch (x.reason) {
    case 'not_running':
      return 'bot 未啟動（群組訊息不會自動啟動它）'
    case 'blocked':
      return 'agent 正在等終端回應'
    case 'in_flight':
      return '上一回合還在進行中'
    case 'unknown_delivery':
      return '上一回合送達狀態未知，請先放棄該回合'
    default:
      return x.detail || x.reason
  }
}
