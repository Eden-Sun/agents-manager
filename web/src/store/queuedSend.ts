import type { Message, Turn } from '../api/types'

/** Server-owned queued turn projected into the shape the composer needs. */
export interface QueuedSend {
  turnId: string
  text: string
  attachments: string[]
}

/** The composer-facing queued row is derived only from daemon turns and their user messages. */
export function queuedSendFor(
  state: { turns: Record<string, Record<string, Turn>>; messages: Record<string, Message[]> },
  botId: string,
): QueuedSend | null {
  const turn = Object.values(state.turns[botId] ?? {})
    .filter((candidate) => candidate.status === 'queued' && candidate.awaitsIdle)
    .sort((a, b) => a.created_at.localeCompare(b.created_at))[0]
  if (!turn) return null
  const message = (state.messages[botId] ?? []).find((candidate) => candidate.turn_id === turn.id && candidate.role === 'user')
  return {
    turnId: turn.id,
    text: message?.content ?? '',
    attachments: message?.attachments.map((attachment) => attachment.id) ?? [],
  }
}

/** 退回的文字接在輸入框現有內容前面；空字串不多加換行。 */
export function prependDraft(text: string, current: string): string {
  return text ? (current ? `${text}\n${current}` : text) : current
}

/** Temporary composer adapter while the UI switches from local queueing to sendPrompt(queueIfBusy). */
export interface ComposerIO {
  setText: (text: string) => void
  clearFiles: () => void
}

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

/** Legacy composer helper; server-owned queues are no longer restored into a browser-side slot. */
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
