import type { Message, Turn } from '../api/types'

/**
 * Transitional composer projection for issue #733. TODO(D): replace this component-side projection
 * with the queued-turn selector exposed by D's store once that selector lands.
 */
export interface QueuedIdleSend {
  turnId: string
  text: string
  attachments: string[]
}

type TurnWithAwaitingIdle = Turn & { awaitsIdle?: boolean }

/** Find the web prompt the daemon has parked until this bot becomes idle. */
export function queuedIdleSend(
  turns: Record<string, Turn> | undefined,
  messages: Message[] | undefined,
): QueuedIdleSend | null {
  const turn = Object.values(turns ?? {})
    .filter((candidate) => candidate.status === 'queued' && (candidate as TurnWithAwaitingIdle).awaitsIdle === true)
    .sort((a, b) => a.created_at.localeCompare(b.created_at))[0]
  if (!turn) return null

  const message = (messages ?? []).find((candidate) => candidate.turn_id === turn.id && candidate.role === 'user')
  return {
    turnId: turn.id,
    text: message?.content ?? '',
    attachments: message?.attachments.map((attachment) => attachment.id) ?? [],
  }
}
