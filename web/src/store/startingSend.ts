/**
 * issue #122：沒在跑的 bot 按送出。訊息先在 daemon 落地成一筆 `queued` turn（`awaits_start`），daemon 自己啟動 bot、
 * 起來後由佇列送出——瀏覽器記憶體不再是唯一的那一份，重整、關分頁、換裝置都不會丟。
 *
 * 這裡只從 store 已有的 turn／訊息推出「現在有一則在等 bot 起來」，純函式，composer 與測試共用。
 */
import type { Message, Turn } from '../api/types'
import { t } from '../lib/i18n'

export interface StartingSend {
  turnId: string
  /** 泡泡原文（使用者打的那段）；找不到訊息時是空字串。 */
  text: string
  attachments: number
  /** 上一次啟動失敗的原因；`null`＝還在啟動（或 daemon 正要啟動）。 */
  startError: string | null
}

/**
 * 這顆 bot 有沒有一則「送出時 bot 沒在跑、正在等它起來」的訊息。有好幾則時取最早排的（daemon 起來後先送它）：
 * 頁面回來的 turns 是 created_at 倒序，不能取插入順序的第一個（#1697）。
 */
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

/** 輸入框上方那一條的說法。bot 已經有 run（又起來了、或正在起）時，上一次的失敗原因不再算數。 */
export function startingSendLabel(s: StartingSend, hasRun = false): string {
  return s.startError && !hasRun
    ? t('沒能啟動（{reason}），還沒送出：', { reason: s.startError })
    : t('啟動中，起來後自動送出：')
}

/**
 * `POST /prompt` 回 `delivery: queued`：先在本地記一筆排隊中的 turn，socket 那一幀慢了輸入框也馬上換成「啟動中」。
 * 已經收到過（frame 先到）就不動——那一份比較新。
 */
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
