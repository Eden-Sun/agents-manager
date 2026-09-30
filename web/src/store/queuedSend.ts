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
