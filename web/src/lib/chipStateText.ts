import { msg, t, tr } from './i18n'

export interface ChipState {
  needsReply: boolean
  unread: number
  waitsKids: boolean
  /** codex 模型滿載（#1674），排在要你回答之後、未讀之前。 */
  atCapacity?: boolean
  working: boolean
  /** 子 agent 有 working／blocked 的數量（#344 補充 4）；跟自己的狀態並存，所以另外接在後面。 */
  kids?: number
  /** 保溫回覆到了、使用者還沒送新 prompt（`lib/keepWarm.ts`）。 */
  keepWarmReplied?: boolean
  /** 按了「不用保溫」（冰凍）：晶片畫雪花，這裡補成字。 */
  noWarm?: boolean
}

/** 晶片上只有顏色／點／數字表達的狀態，補成讀屏念得出來的字（畫面上用 sr-only，不佔版面）。 */
export function chipStateText({ needsReply, unread, waitsKids, working, atCapacity = false, kids = 0, keepWarmReplied = false, noWarm = false }: ChipState): string {
  const own = needsReply
    ? t('（需要回應）')
    : atCapacity
      ? t('（模型滿載）')
      : unread > 0
        ? t('（{n} 個回合未讀）', { n: unread })
        : waitsKids
          ? t('（等子 agent）')
          : working
            ? t('（執行中）')
            : ''
  const kidsPart = kids > 0 ? t('（{text}）', { text: kidsText(kids) }) : ''
  return `${own}${kidsPart}${noWarm ? t('（不保溫）') : ''}${keepWarmReplied ? t('（{text}）', { text: tr(KEEP_WARM_REPLIED_TEXT) }) : ''}`
}

/** 保溫回覆提示的白話：晶片 ♨ 符號之外的文字版（sr-only、tooltip、狀態卡共用）。 */
export const KEEP_WARM_REPLIED_TEXT = msg('保溫回覆已到，送出新 prompt 前維持 ♨ 提示')

/** tooltip／aria 用：「N 個子 agent 在跑」。 */
export function kidsText(n: number): string {
  return t('{n} 個子 agent 在跑', { n })
}
