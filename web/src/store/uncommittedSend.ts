/**
 * `POST /prompt` 的 503「外面做了、DB 那一半還沒寫成」（#149 `delivery_state_uncommitted`、#147 `send_now_state_uncommitted`）。
 *
 * 這不是「沒送出」：turn 與 user 訊息在回 503 之前就寫進 DB 並推了 `message_added`／`turn_updated`，字也已經進了 bot
 * （或 herdr 明確拒收）。前端當它失敗——輸入框留著同一段字、排隊的被放回去——下一次 Enter 或回合結束的 flush 就用新的
 * client_request_id 再送一次，bot 收到兩則（API.md §5：不要換新的 client_request_id 重送）。
 */
import { ApiError } from '../api/types'
import type { Turn, TurnDelivery } from '../api/types'

export interface UncommittedSend {
  /** 字有沒有進去：`false`＝herdr 拒收（一個字都沒進去）；`true`／`null`（不知道）＝已經送出或可能已送出，不能當成沒送。 */
  sent: boolean | null
  turnId: string
  /** daemon 看到的送達結果（`ok`／`unverified`／`unknown`／`failed`）；插隊送出的 503 沒有這一欄。 */
  delivery: string | null
}

/** 這個錯誤是不是「送了、結果還沒寫成」；不是就回 `null`（照一般失敗處理）。 */
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
    ? '訊息沒送進去（herdr 拒收），但結果還沒寫進 daemon 的資料庫；daemon 會自己補上，確認 agent 狀態後可以再送。'
    : '訊息已送出，但送達結果還沒寫進 daemon 的資料庫（daemon 會自己補上）——不要重送。'
}

/**
 * 送出之後先把這一回合記成進行中：輸入框馬上鎖上，socket 那一幀慢了也不會再收一則。
 * 終態（`turn_updated(completed)` 先到）不能被蓋回；已經收到的以那一份為準，只補 `delivery`。
 */
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
      startError: null,
      client_request_id: crid,
      created_at: new Date().toISOString(),
      completed_at: null,
    }),
    delivery,
  }
  return { turns: { ...s.turns, [botId]: { ...(s.turns[botId] ?? {}), [turnId]: turn } } }
}
