import type { Turn } from '../api/types'
import { msg } from './i18n'

export type DeliveryNotice = 'none' | 'hint' | 'warn'

export function deliveryNotice(turn: Pick<Turn, 'delivery' | 'unverified' | 'autoResend'> | undefined | null): DeliveryNotice {
  if (!turn || turn.delivery !== 'ok' || !turn.unverified) return 'none'
  return turn.autoResend ? 'hint' : 'warn'
}

export const DELIVERY_WARN_TEXT = msg(
  '已打字送出，但這個 bot 沒有可以逐字核對的紀錄（grok、遠端主機、codex 尚未回報 session），而且不會自動重送——請到終端分頁確認它真的收到',
)

export const DELIVERY_HINT_TEXT = msg('這一則交給 herdr 轉送，沒有逐字證據；萬一沒進到輸入框，daemon 會自己重送一次。')
