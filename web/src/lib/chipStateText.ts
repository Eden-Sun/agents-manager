import { msg, t, tr } from './i18n'

export interface ChipState {
  needsReply: boolean
  unread: number
  waitsKids: boolean
  atCapacity?: boolean
  working: boolean
  kids?: number
  keepWarmReplied?: boolean
  noWarm?: boolean
}

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

export const KEEP_WARM_REPLIED_TEXT = msg('保溫回覆已到，送出新 prompt 前維持 ♨ 提示')

export function kidsText(n: number): string {
  return t('{n} 個子 agent 在跑', { n })
}
