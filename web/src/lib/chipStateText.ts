export interface ChipState {
  needsReply: boolean
  unread: number
  waitsKids: boolean
  working: boolean
}

/** 晶片上只有顏色／點／數字表達的狀態，補成讀屏念得出來的字（畫面上用 sr-only，不佔版面）。 */
export function chipStateText({ needsReply, unread, waitsKids, working }: ChipState): string {
  if (needsReply) return '（需要回應）'
  if (unread > 0) return `（${unread} 個回合未讀）`
  if (waitsKids) return '（等子 agent）'
  if (working) return '（執行中）'
  return ''
}
