export type SlotHolderKind = 'user' | 'start' | 'daemon' | 'agm' | 'bot' | 'unknown'

export interface SlotHolder {
  kind: SlotHolderKind
  botName: string | null
}

const KINDS: readonly SlotHolderKind[] = ['user', 'start', 'daemon', 'agm', 'bot']

/** daemon 的 409 `queue_slot_taken` 裡的 `holder`；舊 daemon 沒帶＝`null`。 */
export function slotHolderFrom(body: Record<string, unknown>): SlotHolder | null {
  const h = body.holder
  if (!h || typeof h !== 'object') return null
  const rec = h as Record<string, unknown>
  const kind = KINDS.find((k) => k === rec.kind) ?? 'unknown'
  return { kind, botName: typeof rec.bot_name === 'string' && rec.bot_name ? rec.bot_name : null }
}

/** 唯一的排隊槽被佔著、這一則被退回時的說明（輸入框的字一直還在）。 */
export function queueSlotNotice(holder: SlotHolder | null, attachmentCount: number): string {
  const note = attachmentCount > 0 ? `；${attachmentCount} 個附件要重新加` : ''
  switch (holder?.kind) {
    case 'agm':
      return `AGM 的交辦正在排隊，送出後才能排下一則；你這一則留在輸入框${note}`
    case 'bot':
      return `${holder.botName ? `「${holder.botName}」` : '另一顆 bot'} 的訊息正在排隊，送出後才能排下一則；你這一則留在輸入框${note}`
    case 'start':
      return `上一則訊息還在等這顆 bot 啟動，送出後才能排下一則；你這一則留在輸入框${note}`
    case 'daemon':
      return `daemon 的通知正在排隊，送出後才能排下一則；你這一則留在輸入框${note}`
    default:
      return `已有一則訊息排隊中，這一則已退回輸入框${note}`
  }
}
