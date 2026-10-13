import type { OccurrenceStatus, Schedule } from '../api/schedules'

/** 下次執行時間，附上排程自己的時區名稱（不把 UTC 字串直接丟給使用者）。 */
export function formatDue(iso: string | null, tz: string): string {
  if (!iso) return '沒有下一次（已完成或已停用）'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const when = d.toLocaleString('zh-TW', { timeZone: tz, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
  return `${when}（${tz}）`
}

export const OCCURRENCE_LABEL: Record<OccurrenceStatus, string> = {
  pending: '等待送出',
  queued: '已排隊，等待 Bot 接收',
  done: '已完成',
  failed: '失敗',
  cancelled: '已取消',
  unknown: '送出結果不明，需要人工確認',
}

const WAIT_LABEL: Record<string, string> = {
  bot_busy: 'Bot 忙碌，前面已有排隊的回合',
  stopping: 'Bot 正在停止，停完後再送',
}

/** 未結束的 occurrence 要顯示什麼：依 pending 的等待原因、queued 綁定 turn 的真實狀態。 */
export function occurrenceLine(o: { status: OccurrenceStatus; waiting_reason?: string | null; turn_status?: string | null }, liveTurnStatus?: string): string {
  if (o.status === 'pending') return o.waiting_reason ? WAIT_LABEL[o.waiting_reason] ?? '等待中' : '等待送出'
  if (o.status === 'queued') {
    // 即時狀態（store 的 turn_updated）優先；沒有就用列表回來的快照。
    const t = liveTurnStatus ?? o.turn_status
    if (t === 'in_flight') return '執行中'
    if (t === 'completed' || t === 'completed_fallback' || t === 'failed') return '回合已結束，結果同步中…'
    // 沒有投影 start_error／quota_hold：不猜原因。額度、離線、啟動失敗的原因請到 Bot 對話看。
    return '已排隊，等待 Bot 接收'
  }
  return OCCURRENCE_LABEL[o.status]
}

/** 列表上的一行狀態：停用／已完成／未結束的 occurrence。 */
export function scheduleStatusLine(s: Schedule, liveTurnStatus?: string): string {
  if (s.enabled === false) return '已停用'
  if (s.open) return occurrenceLine(s.open, liveTurnStatus)
  if (!s.next_due) return '已完成'
  return `下次 ${formatDue(s.next_due, s.tz)}`
}

/** 用瀏覽器時區當預設；不支援 supportedValuesOf 時退回 UTC。 */
export function defaultTimeZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'
  } catch {
    return 'UTC'
  }
}

export function timeZoneOptions(current: string): string[] {
  const list = (Intl as unknown as { supportedValuesOf?: (k: string) => string[] }).supportedValuesOf?.('timeZone') ?? []
  return list.includes(current) ? list : [current, ...list]
}
