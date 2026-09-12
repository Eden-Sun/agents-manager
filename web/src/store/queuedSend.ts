/**
 * 排隊訊息（`queuedSends`）送失敗後怎麼放回去。
 *
 * 送出路徑（回合結束自動送、「中止並取代」、「併行送入」）原本都是**先把佇列清掉再送**，
 * `sendPrompt` 回 false（409 `picker_open`／`dialog_open`／`needs_login`、502、網路錯）時
 * 只有 toast，文字與附件 id 就永久不見了（docs/reviews/2026-09-12/web.md §1 flushQueued）。
 *
 * 純函式、不碰 store：三個入口共用同一條規則，也方便直接測。
 */

export interface QueuedSend {
  text: string
  attachments: string[]
}

export interface QueuedSendSlots {
  queuedSends: Record<string, QueuedSend>
  drafts: Record<string, string>
}

export interface RestoreResult {
  patch: Partial<QueuedSendSlots>
  /** 放回輸入框而不是佇列時，附件跟不回去——回傳數量讓呼叫端講一聲。 */
  droppedAttachments: number
}

/**
 * - 佇列槽位空著（正常情況）：原樣排回去，下一次回合結束會再試，附件 id 也還在。
 * - 槽位已被使用者剛排的新一則佔走：不能蓋掉新的，把舊文字接回輸入框最前面；
 *   附件 id 只存在佇列裡，這條路救不回來，用 `droppedAttachments` 回報。
 */
export function restoreQueued(s: QueuedSendSlots, botId: string, pending: QueuedSend): RestoreResult {
  if (!s.queuedSends[botId]) {
    return { patch: { queuedSends: { ...s.queuedSends, [botId]: pending } }, droppedAttachments: 0 }
  }
  const key = `bot:${botId}`
  const cur = s.drafts[key] ?? ''
  const text = pending.text ? (cur ? `${pending.text}\n${cur}` : pending.text) : cur
  return {
    patch: text === cur ? {} : { drafts: { ...s.drafts, [key]: text } },
    droppedAttachments: pending.attachments.length,
  }
}
