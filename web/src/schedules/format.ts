import type { OccurrenceStatus, Schedule } from '../api/schedules'
import { intlLocale, msg, t, tr } from '../lib/i18n'

const DUE_OPTIONS: Intl.DateTimeFormatOptions = { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false }

/**
 * 依 `tz` 排版；瀏覽器 Intl 不認識 `tz`（後端接受的合法時區，tzdb 版本差）時退回 UTC，並標示不支援（#1812）。
 * `ok` 為 false 表示退回了 UTC（字串已自帶原時區名）。
 */
function zonedText(d: Date, tz: string, options: Intl.DateTimeFormatOptions): { text: string; ok: boolean } {
  try {
    return { text: d.toLocaleString(intlLocale(), { ...options, timeZone: tz }), ok: true }
  } catch (e) {
    if (!(e instanceof RangeError)) throw e
    return { text: t('{time} UTC（{tz} 這台瀏覽器不支援，以 UTC 顯示）', { time: d.toLocaleString(intlLocale(), { ...options, timeZone: 'UTC' }), tz }), ok: false }
  }
}

/** 排程時區的時間字串，附上時區名（下次執行用）。無效日期原樣回傳。 */
export function formatZoned(iso: string, tz: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const { text, ok } = zonedText(d, tz, DUE_OPTIONS)
  return ok ? t('{text}（{tz}）', { text, tz }) : text
}

/** 最近執行列的時間：沿用改動前的預設選項（含秒、不附時區名）；瀏覽器不認識時區才退回 UTC 並標示。 */
export function formatOccurrenceTime(iso: string, tz: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return zonedText(d, tz, {}).text
}

/** 下次執行時間，附上排程自己的時區名稱（不把 UTC 字串直接丟給使用者）。 */
export function formatDue(iso: string | null, tz: string): string {
  if (!iso) return t('沒有下一次（已完成或已停用）')
  return formatZoned(iso, tz)
}

export const OCCURRENCE_LABEL: Record<OccurrenceStatus, string> = {
  pending: msg('等待送出'),
  queued: msg('已排隊，等待 Bot 接收'),
  done: msg('已完成'),
  failed: msg('失敗'),
  cancelled: msg('已取消'),
  unknown: msg('送出結果不明，需要人工確認'),
}

const WAIT_LABEL: Record<string, string> = {
  bot_busy: msg('Bot 忙碌，前面已有排隊的回合'),
  stopping: msg('Bot 正在停止，停完後再送'),
}

/** 未結束的 occurrence 要顯示什麼：依 pending 的等待原因、queued 綁定 turn 的真實狀態。 */
export function occurrenceLine(o: { status: OccurrenceStatus; waiting_reason?: string | null; turn_status?: string | null; turn_awaits_start?: number | null; turn_start_error?: string | null }, liveTurnStatus?: string): string {
  if (o.status === 'pending') return o.waiting_reason ? tr(WAIT_LABEL[o.waiting_reason] ?? msg('等待中')) : t('等待送出')
  if (o.status === 'queued') {
    // 即時狀態（store 的 turn_updated）優先；沒有就用列表回來的快照。
    const st = liveTurnStatus ?? o.turn_status
    if (st === 'in_flight') return t('執行中')
    if (st === 'completed' || st === 'completed_fallback' || st === 'failed') return t('回合已結束，結果同步中…')
    if (st === 'queued' && o.turn_awaits_start === 1 && o.turn_start_error) return t('Bot 啟動失敗（尚未送出）：{reason}', { reason: o.turn_start_error })
    // 沒有投影 start_error／quota_hold：不猜原因。額度、離線、啟動失敗的原因請到 Bot 對話看。
    return t('已排隊，等待 Bot 接收')
  }
  return tr(OCCURRENCE_LABEL[o.status])
}

/** 列表上的一行狀態：停用／已完成／未結束的 occurrence。 */
export function scheduleStatusLine(s: Schedule, liveTurnStatus?: string): string {
  if (s.enabled === false) return t('已停用')
  if (s.open) return occurrenceLine(s.open, liveTurnStatus)
  if (!s.next_due) return t('已完成')
  return t('下次 {due}', { due: formatDue(s.next_due, s.tz) })
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
  const set = new Set<string>()
  if (current) set.add(current)
  set.add('UTC')
  for (const tz of list) {
    set.add(tz)
  }
  return Array.from(set)
}

