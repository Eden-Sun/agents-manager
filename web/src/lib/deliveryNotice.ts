import type { Turn } from '../api/types'

/**
 * 使用者要看到的送達提示（AGM 2026-09-16 裁示，接 c1ed6e2）。
 *
 * 這個標記對使用者的意思是「這則可能悄悄沒送到，而且沒有人會再試一次」。所以分兩種：
 * * `warn`：沒有證據**而且**不會自動重送（打過字、證不明，例如 grok 多行）——只有這種要標出來。
 * * `hint`：沒有證據但**會自動重送**（herdr `agent.prompt`）——有安全網，只放在 hover／詳情，
 *   不佔使用者的注意力；claude 多數 prompt 走這條，全標只會讓真正該看的那筆被淹掉。
 * * `none`：有證據，或這一則的狀態由 `delivery` 自己說（`unknown` / `failed` 另有顯示）。
 */
export type DeliveryNotice = 'none' | 'hint' | 'warn'

export function deliveryNotice(turn: Pick<Turn, 'delivery' | 'unverified' | 'autoResend'> | undefined | null): DeliveryNotice {
  if (!turn || turn.delivery !== 'ok' || !turn.unverified) return 'none'
  return turn.autoResend ? 'hint' : 'warn'
}

export const DELIVERY_WARN_TEXT =
  '已打字送出，但這個 bot 沒有可以逐字核對的紀錄（grok、遠端主機、codex 尚未回報 session），而且不會自動重送——請到終端分頁確認它真的收到'

export const DELIVERY_HINT_TEXT =
  '這一則交給 herdr 轉送，沒有逐字證據；萬一沒進到輸入框，daemon 會自己重送一次。'
