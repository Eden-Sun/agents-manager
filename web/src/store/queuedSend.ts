import type { Message, Turn } from '../api/types'

export interface QueuedSend {
  turnId: string
  text: string
  attachments: string[]
}

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

export function prependDraft(text: string, current: string): string {
  return text ? (current ? `${text}\n${current}` : text) : current
}
