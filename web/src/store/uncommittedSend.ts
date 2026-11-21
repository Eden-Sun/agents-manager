import { ApiError } from '../api/types'
import type { Turn, TurnDelivery } from '../api/types'
import { t } from '../lib/i18n'

export interface UncommittedSend {
  sent: boolean | null
  turnId: string
  delivery: string | null
}

export function asUncommittedSend(e: unknown): UncommittedSend | null {
  if (!(e instanceof ApiError) || e.status !== 503) return null
  const code = e.body.error
  if (code !== 'delivery_state_uncommitted' && code !== 'send_now_state_uncommitted') return null
  const turnId = typeof e.body.turn_id === 'string' ? e.body.turn_id : ''
  if (!turnId) return null
  return {
    sent: e.body.sent === false ? false : e.body.sent === true ? true : null,
    turnId,
    delivery: typeof e.body.delivery === 'string' ? e.body.delivery : null,
  }
}

export function uncommittedSendText(u: UncommittedSend): string {
  return u.sent === false
    ? t('訊息沒送進去（herdr 拒收），但結果還沒寫進 daemon 的資料庫；daemon 會自己補上，確認 agent 狀態後可以再送。')
    : t('訊息已送出，但送達結果還沒寫進 daemon 的資料庫（daemon 會自己補上）——不要重送。')
}

export function noteInFlightTurn(
  s: { turns: Record<string, Record<string, Turn>> },
  botId: string,
  turnId: string,
  crid: string,
  runId: string | null,
  delivery: TurnDelivery,
): { turns: Record<string, Record<string, Turn>> } | Record<string, never> {
  const existing = s.turns[botId]?.[turnId]
  if (existing && existing.status !== 'in_flight') return {}
  const turn: Turn = {
    ...(existing ?? {
      id: turnId,
      conversation_id: '',
      run_id: runId,
      bot_id: botId,
      origin: 'web' as const,
      status: 'in_flight' as const,
      unverified: false,
      autoResend: true,
      awaitsStart: false,
      awaitsIdle: false,
      startError: null,
      client_request_id: crid,
      created_at: new Date().toISOString(),
      completed_at: null,
    }),
    delivery,
  }
  return { turns: { ...s.turns, [botId]: { ...(s.turns[botId] ?? {}), [turnId]: turn } } }
}
