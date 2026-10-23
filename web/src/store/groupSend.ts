import type { GroupChatResult } from '../api/types'
import { t } from '../lib/i18n'

/** 一顆都沒送到（全被跳過）＝ false。舊 daemon 沒有 `delivered` 欄，退回看 `sent`。 */
export function groupSendDelivered(res: Pick<GroupChatResult, 'delivered' | 'sent'>): boolean {
  return res.delivered ?? res.sent.length > 0
}

/** 跳過的收件者為什麼沒送到，給 toast 用（daemon 的 `detail` 是英文原文；機器碼 §13.3 翻成人話）。 */
export function groupSkipText(x: { reason: string; detail: string }): string {
  switch (x.reason) {
    case 'not_running':
      return t('bot 未啟動（群組訊息不會自動啟動它）')
    case 'blocked':
      return t('agent 正在等終端回應')
    case 'in_flight':
      return t('上一回合還在進行中')
    case 'unknown_delivery':
      return t('上一回合送達狀態未知，請先放棄該回合')
    // 下面四個 daemon 的 detail 常是機器碼或整段 JSON（`human_of` 退回 body），一律用固定的人話，不貼 detail。
    case 'unprocessable':
      return t('這則訊息對方無法確認送達（多半是太長），請縮短後再送')
    case 'forbidden':
      return t('沒有權限替這顆 bot 轉述')
    case 'unavailable':
      return t('維護窗口的狀態暫時讀不到，先不送；稍後再試')
    case 'uncommitted':
      return t('送出的結果還不確定，請到對話裡確認')
    default:
      return x.detail || x.reason
  }
}
