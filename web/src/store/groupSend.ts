import type { GroupChatResult } from '../api/types'

/** 一顆都沒送到（全被跳過）＝ false。舊 daemon 沒有 `delivered` 欄，退回看 `sent`。 */
export function groupSendDelivered(res: Pick<GroupChatResult, 'delivered' | 'sent'>): boolean {
  return res.delivered ?? res.sent.length > 0
}
