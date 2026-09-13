/**
 * 排隊訊息送失敗後放回去：送出路徑先清佇列再送，失敗時文字與附件會永久遺失
 * （docs/reviews/2026-09-12/web.md §1 flushQueued）。純函式，三個入口共用。
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
  /** 放回輸入框時丟掉的附件數，讓呼叫端講一聲。 */
  droppedAttachments: number
}

/** 槽位空著就排回去；已被新一則佔走則不蓋掉，舊文字接回輸入框最前面（附件救不回）。 */
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
