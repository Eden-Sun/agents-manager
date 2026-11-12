import type { OccurrenceStatus, Schedule, ScheduleOccurrence } from '../api/schedules'
import { getLang, intlLocale, msg, t, tr } from '../lib/i18n'
import { formatDuration, totalDurationMs } from './duration'

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
export function formatZoned(iso: string, tz: string, seconds = false): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const { text, ok } = zonedText(d, tz, seconds ? { ...DUE_OPTIONS, second: '2-digit' } : DUE_OPTIONS)
  return ok ? t('{text}（{tz}）', { text, tz }) : text
}

/** 最近執行列的時間：沿用改動前的預設選項（含秒、不附時區名）；瀏覽器不認識時區才退回 UTC 並標示。 */
export function formatOccurrenceTime(iso: string, tz: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return zonedText(d, tz, {}).text
}

/** 下次執行時間，附上排程自己的時區名稱（不把 UTC 字串直接丟給使用者）。 */
export function formatDue(iso: string | null, tz: string, seconds = false): string {
  if (!iso) return t('沒有下一次（已完成或已停用）')
  return formatZoned(iso, tz, seconds)
}

/** 每天／每週排程裡的隨機區間時段（有 `until` 的）。 */
export function randomRanges(spec: unknown): { time: string; until: string }[] {
  const slots = (spec as { slots?: { time?: string; until?: string | null }[] } | null)?.slots
  return (slots ?? []).flatMap((s) => (s.time && s.until ? [{ time: s.time, until: s.until }] : []))
}

/** `08:55–09:00 隨機`（秒為 00 就不顯示）；多個區間用頓號接起來。沒有隨機區間回空字串。 */
export function randomSummary(spec: unknown): string {
  return randomRanges(spec).map((r) => t('{range} 隨機', { range: `${clockHm(r.time)}–${clockHm(r.until)}` })).join('、')
}

/** `HH:MM:00` 收成 `HH:MM`；其他原樣。 */
function clockHm(v: string): string {
  return v.length === 8 && v.endsWith(':00') ? v.slice(0, 5) : v
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
export function occurrenceLine(o: { status: OccurrenceStatus; waiting_reason?: string | null; turn_status?: string | null; turn_awaits_start?: number | null; turn_start_error?: string | null; next_retry_at?: string | null; attempt_no?: number }, liveTurnStatus?: string): string {
  if (o.status === 'pending' && o.next_retry_at) return t('等待重試（第 {n} 次執行）', { n: o.attempt_no ?? 1 })
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
  const range = randomSummary(s.spec)
  const next = t('下次 {due}', { due: formatDue(s.next_due, s.tz, range !== '') })
  return range ? `${range} · ${next}` : next
}

/** 側欄列的短時間：`11/02 08:33`；排程時區跟瀏覽器不同才補時區縮寫。完整字串放 title（`scheduleStatusLine`）。 */
export function formatDueShort(iso: string, tz: string, seconds = false): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  try {
    const parts = new Intl.DateTimeFormat('en-US', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', ...(seconds ? { second: '2-digit' as const } : {}), hourCycle: 'h23', timeZone: tz, ...(tz === defaultTimeZone() ? {} : { timeZoneName: 'short' }) }).formatToParts(d)
    const p = (type: string) => parts.find((x) => x.type === type)?.value ?? ''
    const zone = p('timeZoneName')
    return `${p('month')}/${p('day')} ${p('hour')}:${p('minute')}${seconds ? `:${p('second')}` : ''}${zone ? ` ${zone}` : ''}`
  } catch (e) {
    if (!(e instanceof RangeError)) throw e
    return formatZoned(iso, tz)
  }
}

/** 側欄一行的短狀態（2026-11-01 使用者：排程列「該優化」）：停用／已完成／未結束的 occurrence／下次短時間。 */
export function scheduleStatusShort(s: Schedule, liveTurnStatus?: string): string {
  if (s.enabled === false) return t('已停用')
  if (s.open) return occurrenceLine(s.open, liveTurnStatus)
  if (!s.next_due) return t('已完成')
  const range = randomSummary(s.spec)
  return range ? `${range} · ${formatDueShort(s.next_due, s.tz, true)}` : formatDueShort(s.next_due, s.tz)
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


/* 專案排程列（2026-11-12）：固定三行——名稱（狀態點）／下次或現在／上次。每行只放一件事，完整資訊放 title。 */

const ZH_WEEKDAY = ['一', '二', '三', '四', '五', '六', '日']
const EN_WEEKDAY = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']

/** 後端接受、但瀏覽器不認得的時區退回 UTC（同 zonedText 的做法）。 */
function zoneOf(tz: string): string {
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: tz })
    return tz
  } catch {
    return 'UTC'
  }
}

interface ZonedParts { y: number; m: number; d: number; hm: string; wd: number }

/** 某時刻在某時區的年月日、時分、星期（0=週一）。 */
function zonedParts(d: Date, zone: string): ZonedParts {
  const p = new Intl.DateTimeFormat('en-US', { timeZone: zone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hourCycle: 'h23', weekday: 'short' }).formatToParts(d)
  const v = (type: string) => p.find((x) => x.type === type)?.value ?? ''
  return { y: Number(v('year')), m: Number(v('month')), d: Number(v('day')), hm: `${v('hour')}:${v('minute')}`, wd: EN_WEEKDAY.indexOf(v('weekday')) }
}

/** 時區縮寫：只有排程時區跟瀏覽器不同才回傳，相同回空字串。 */
function zoneTag(d: Date, zone: string): string {
  if (zone === defaultTimeZone()) return ''
  const part = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'short' }).formatToParts(d).find((x) => x.type === 'timeZoneName')
  return part?.value ?? zone
}

function weekdayName(i: number): string {
  return getLang() === 'en' ? EN_WEEKDAY[i] : ZH_WEEKDAY[i]
}

/**
 * 列上的一個時間：今天／明天／昨天；7 天內「11/16（一）」；更遠「11/16」；跨年加年；只到分鐘（24 小時制）。
 * 排程時區跟瀏覽器不同才在時間後補縮寫。`now` 可注入，方便測試。
 */
export function whenText(iso: string, tz: string, now: Date = new Date()): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const zone = zoneOf(tz)
  const at = zonedParts(d, zone)
  const today = zonedParts(now, zone)
  const diff = Math.round((Date.UTC(at.y, at.m - 1, at.d) - Date.UTC(today.y, today.m - 1, today.d)) / 86_400_000)
  const year = at.y !== today.y ? `${at.y}/` : ''
  const md = `${year}${String(at.m).padStart(2, '0')}/${String(at.d).padStart(2, '0')}`
  let day: string
  if (diff === 0) day = t('今天')
  else if (diff === 1) day = t('明天')
  else if (diff === -1) day = t('昨天')
  else if (Math.abs(diff) < 7) day = t('{md}（{wd}）', { md, wd: weekdayName(at.wd) })
  else day = md
  const tag = zoneTag(d, zone)
  return `${day} ${at.hm}${tag ? ` ${tag}` : ''}`
}

/** 固定時間的排程描述（停用時「原訂…」用）：每天 18:01–18:05、每週一、三 09:00、每 1 分 1 秒、一次性的時間。 */
export function cadenceText(s: Schedule): string {
  const spec = (s.spec ?? {}) as { time?: string; at?: string; every_secs?: number; weekdays?: number[]; slots?: { time?: string; until?: string | null }[] }
  if (s.mode === 'interval') return t('每 {d}', { d: formatDuration((spec.every_secs ?? 0) * 1000) })
  if (s.mode === 'once') return String(spec.at ?? '').replace('T', ' ')
  const slots = (spec.slots ?? []).filter((x) => x.time)
  const list = slots.length ? slots : [{ time: spec.time, until: null }]
  const times = list.map((x) => (x.time && x.until ? `${clockHm(x.time)}–${clockHm(x.until)}` : clockHm(x.time ?? ''))).join('、')
  if (s.mode === 'weekly') {
    const days = [...(spec.weekdays ?? [])].sort((a, b) => a - b).map(weekdayName).join(getLang() === 'en' ? ', ' : '、')
    return t('每週{days} {times}', { days, times })
  }
  return t('每天 {times}', { times })
}

/** 執行中（或綁定的 turn 進行中）的顯示：執行中 · 已 25 秒；等待 Bot 忙碌；其餘沿用 occurrenceLine。 */
function openText(o: ScheduleOccurrence, liveTurnStatus: string | undefined, nowMs: number): string {
  const st = liveTurnStatus ?? o.turn_status
  if (o.running_since || st === 'in_flight') {
    const ms = totalDurationMs({ duration_ms: o.duration_ms ?? 0, running_since: o.running_since }, nowMs) ?? 0
    return t('執行中 · 已 {d}', { d: formatDuration(ms) })
  }
  if (o.status === 'pending' && o.waiting_reason === 'bot_busy') return t('等待 Bot（忙碌中）')
  return occurrenceLine(o, liveTurnStatus)
}

export interface RowLine { label?: string; text: string; title: string; tone?: 'needs-rebind' }
export type RowDot = 'ok' | 'fail' | 'never' | 'off'

/**
 * 專案排程列的三行內容。第1行（名稱＋狀態點）在元件裡組；這裡給第2行（下次／現在／停用／需重指定）與第3行（上次）。
 * 狀態點：停用＝空心（off）；否則看上次結果：成功 ok、失敗 fail、從未執行 never。
 */
export function scheduleRowLines(s: Schedule, liveTurnStatus: string | undefined, nowMs: number): { dot: RowDot; next: RowLine; last: RowLine } {
  const now = new Date(nowMs)
  const random = randomRanges(s.spec)
  const rangeText = random.map((r) => `${clockHm(r.time)}–${clockHm(r.until)}`).join('、')
  let next: RowLine
  if (s.bot_available === false) {
    next = { text: t('⚠ 需要重新指定 Bot'), title: t('⚠ 需要重新指定 Bot'), tone: 'needs-rebind' }
  } else if (s.enabled === false) {
    const c = cadenceText(s)
    next = { text: `${t('已停用')} · ${t('原訂{c}', { c })}`, title: `${t('已停用')} · ${t('原訂{c}', { c })}` }
  } else if (s.open) {
    const text = openText(s.open, liveTurnStatus, nowMs)
    next = { label: t('現在'), text, title: text }
  } else if (!s.next_due) {
    next = { text: t('已完成，沒有下一次'), title: t('已完成，沒有下一次') }
  } else {
    const when = whenText(s.next_due, s.tz, now)
    const text = random.length ? `${when}${t('（隨機）')}` : when
    const tip = [formatZoned(s.next_due, s.tz, random.length > 0), random.length ? t('隨機區間 {r}', { r: rangeText }) : ''].filter(Boolean).join(' · ')
    next = { label: t('下次'), text, title: tip }
  }

  return { dot: rowDot(s), next, last: lastLine(s, now) }
}

function scheduleLastOk(s: Schedule): boolean {
  const o = s.last
  return !!o && (o.outcome === 'success' || (o.outcome == null && o.status === 'done'))
}

function rowDot(s: Schedule): RowDot {
  if (s.enabled === false) return 'off'
  if (!s.last || s.last.status === 'cancelled') return 'never'
  return scheduleLastOk(s) ? 'ok' : 'fail'
}

/** 第3行：上次結果、完成時間（附時區，tooltip 完整）、花費時間；沒執行過就寫「還沒執行過」。 */
function lastLine(s: Schedule, now: Date): RowLine {
  const o = s.last
  const label = t('上次')
  if (!o) return { label, text: t('還沒執行過'), title: t('還沒執行過') }
  if (o.status === 'cancelled') return { label, text: t('已取消'), title: t('已取消') }
  const ok = scheduleLastOk(s)
  const mark = ok ? t('✓ 成功') : o.outcome === 'unknown' ? t('? 結果不明') : t('✕ 失敗')
  const at = o.started_at ?? o.updated_at
  const dur = o.duration_ms != null ? formatDuration(o.duration_ms) : null
  const text = [mark, whenText(at, s.tz, now), dur ? t('花 {d}', { d: dur }) : null].filter(Boolean).join(' · ')
  const title = [mark, formatZoned(at, s.tz, true), dur ? t('花 {d}', { d: dur }) : null].filter(Boolean).join(' · ')
  return { label, text, title }
}
