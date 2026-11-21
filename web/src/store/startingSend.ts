import type { Message, Turn } from '../api/types'
import { t } from '../lib/i18n'

export interface StartingSend {
  turnId: string
  text: string
  attachments: number
  startError: string | null
}

export function startingSend(turns: Record<string, Turn> | undefined, messages: Message[] | undefined): StartingSend | null {
  const turn = Object.values(turns ?? {})
    .filter((t) => t.status === 'queued' && t.awaitsStart)
    .sort((a, b) => a.created_at.localeCompare(b.created_at))[0]
  if (!turn) return null
  const msg = (messages ?? []).find((m) => m.turn_id === turn.id && m.role === 'user')
  return {
    turnId: turn.id,
    text: msg?.content ?? '',
    attachments: msg?.attachments.length ?? 0,
    startError: turn.startError,
  }
}

export function startingSendLabel(s: StartingSend, hasRun = false): string {
  return s.startError && !hasRun
    ? t('沒能啟動（{reason}），還沒送出：', { reason: s.startError })
    : t('啟動中，起來後自動送出：')
}

export function noteQueuedTurn(
  s: { turns: Record<string, Record<string, Turn>> },
  botId: string,
  turnId: string,
  crid: string,
  awaitsStart: boolean,
  awaitsIdle = false,
): { turns: Record<string, Record<string, Turn>> } | Record<string, never> {
  if (s.turns[botId]?.[turnId]) return {}
  const turn: Turn = {
    id: turnId,
    conversation_id: '',
    run_id: null,
    bot_id: botId,
    origin: 'web',
    status: 'queued',
    delivery: 'pending',
    unverified: false,
    autoResend: true,
    awaitsStart,
    awaitsIdle,
    startError: null,
    client_request_id: crid,
    created_at: new Date().toISOString(),
    completed_at: null,
  }
  return { turns: { ...s.turns, [botId]: { ...(s.turns[botId] ?? {}), [turnId]: turn } } }
}
