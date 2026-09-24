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

/** 退回的那段接在輸入框現有文字前面；有一邊是空的就不多一個換行。 */
export function prependDraft(text: string, cur: string): string {
  return text ? (cur ? `${text}\n${cur}` : text) : cur
}

/** 槽位空著就排回去；已被新一則佔走則不蓋掉，舊文字接回輸入框最前面（附件救不回）。 */
export function restoreQueued(s: QueuedSendSlots, botId: string, pending: QueuedSend): RestoreResult {
  if (!s.queuedSends[botId]) {
    return { patch: { queuedSends: { ...s.queuedSends, [botId]: pending } }, droppedAttachments: 0 }
  }
  const key = `bot:${botId}`
  const cur = s.drafts[key] ?? ''
  const text = prependDraft(pending.text, cur)
  return {
    patch: text === cur ? {} : { drafts: { ...s.drafts, [key]: text } },
    droppedAttachments: pending.attachments.length,
  }
}

/** 輸入框那一側：元件的 `setText`（寫 store 草稿）與附件列。 */
export interface ComposerIO {
  setText: (text: string) => void
  clearFiles: () => void
}

/**
 * 回合還在跑時按 Enter。**先清輸入框再排隊**：槽位被佔時 `queueSend` 會把前一則接回草稿，
 * 順序反過來，那一則馬上又被這裡的 `setText('')` 清掉——佇列只剩第二則、輸入框是空的，
 * 通知卻說「已退回輸入框」（第二輪 review H1）。
 */
export function queueFromComposer(
  io: ComposerIO & { queueSend: (botId: string, text: string, attachments: string[]) => void },
  botId: string,
  body: string,
  attachments: string[],
): void {
  io.setText('')
  io.clearFiles()
  io.queueSend(botId, body, attachments)
}

/**
 * 「中止並取代」「併行送入」送完之後怎麼收。送出去的是**排隊那一則**時，輸入框裡是另一段草稿
 * （常常正是被退回的上一則），清掉就是再丟一次；沒送成則把排隊那則放回去。
 */
export function settleComposerSend(
  io: ComposerIO & { restoreQueuedSend: (botId: string, pending: QueuedSend) => void },
  botId: string,
  wasQueued: QueuedSend | null,
  ok: boolean,
): void {
  if (ok) {
    if (wasQueued) return
    io.setText('')
    io.clearFiles()
    return
  }
  if (wasQueued) io.restoreQueuedSend(botId, wasQueued)
}
