export interface ChipState {
  needsReply: boolean
  unread: number
  waitsKids: boolean
  working: boolean
  /** 子 agent 有 working／blocked 的數量（#344 補充 4）；跟自己的狀態並存，所以另外接在後面。 */
  kids?: number
  /** 保溫回覆到了、使用者還沒送新 prompt（`lib/keepWarm.ts`）。 */
  keepWarmReplied?: boolean
}

/** 晶片上只有顏色／點／數字表達的狀態，補成讀屏念得出來的字（畫面上用 sr-only，不佔版面）。 */
export function chipStateText({ needsReply, unread, waitsKids, working, kids = 0, keepWarmReplied = false }: ChipState): string {
  const own = needsReply ? '（需要回應）' : unread > 0 ? `（${unread} 個回合未讀）` : waitsKids ? '（等子 agent）' : working ? '（執行中）' : ''
  const kidsPart = kids > 0 ? `（${kidsText(kids)}）` : ''
  return `${own}${kidsPart}${keepWarmReplied ? `（${KEEP_WARM_REPLIED_TEXT}）` : ''}`
}

/** 保溫回覆提示的白話：晶片框變色之外的文字版（sr-only、tooltip、狀態卡共用）。 */
export const KEEP_WARM_REPLIED_TEXT = '保溫回覆已到，送出新 prompt 前維持這個框色'

/** tooltip／aria 用：「N 個子 agent 在跑」。 */
export function kidsText(n: number): string {
  return `${n} 個子 agent 在跑`
}
