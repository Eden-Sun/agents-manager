export interface ChipState {
  needsReply: boolean
  unread: number
  waitsKids: boolean
  working: boolean
  /** 子 agent 有 working／blocked 的數量（#344 補充 4）；跟自己的狀態並存，所以另外接在後面。 */
  kids?: number
}

/** 晶片上只有顏色／點／數字表達的狀態，補成讀屏念得出來的字（畫面上用 sr-only，不佔版面）。 */
export function chipStateText({ needsReply, unread, waitsKids, working, kids = 0 }: ChipState): string {
  const own = needsReply ? '（需要回應）' : unread > 0 ? `（${unread} 個回合未讀）` : waitsKids ? '（等子 agent）' : working ? '（執行中）' : ''
  return kids > 0 ? `${own}（${kidsText(kids)}）` : own
}

/** tooltip／aria 用：「N 個子 agent 在跑」。 */
export function kidsText(n: number): string {
  return `${n} 個子 agent 在跑`
}
