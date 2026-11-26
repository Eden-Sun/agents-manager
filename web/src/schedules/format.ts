import type { OccurrenceStatus, Schedule, ScheduleOccurrence } from '../api/schedules'
import { getLang, intlLocale, msg, t, tr } from '../lib/i18n'
import { formatDuration, totalDurationMs } from './duration'

const DUE_OPTIONS: Intl.DateTimeFormatOptions = { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false }

function zonedText(d: Date, tz: string, options: Intl.DateTimeFormatOptions): { text: string; ok: boolean } {
  try {
    return { text: d.toLocaleString(intlLocale(), { ...options, timeZone: tz }), ok: true }
  } catch (e) {
    if (!(e instanceof RangeError)) throw e
    return { text: t('{time} UTC（{tz} 這台瀏覽器不支援，以 UTC 顯示）', { time: d.toLocaleString(intlLocale(), { ...options, timeZone: 'UTC' }), tz }), ok: false }
  }
}

export function formatZoned(iso: string, tz: string, seconds = false): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  const { text, ok } = zonedText(d, tz, seconds ? { ...DUE_OPTIONS, second: '2-digit' } : DUE_OPTIONS)
  return ok ? t('{text}（{tz}）', { text, tz }) : text
}

export function formatOccurrenceTime(iso: string, tz: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return zonedText(d, tz, {}).text
}

export function formatDue(iso: string | null, tz: string, seconds = false): string {
  if (!iso) return t('沒有下一次（已完成或已停用）')
  return formatZoned(iso, tz, seconds)
}

export function randomRanges(spec: unknown): { time: string; until: string }[] {
  const slots = (spec as { slots?: { time?: string; until?: string | null }[] } | null)?.slots
  return (slots ?? []).flatMap((s) => (s.time && s.until ? [{ time: s.time, until: s.until }] : []))
}

export function randomSummary(spec: unknown): string {
  return randomRanges(spec).map((r) => t('{range} 隨機', { range: `${clockHm(r.time)}–${clockHm(r.until)}` })).join('、')
}

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
  base_building: msg('基準快照建立中，建好後照常執行'),
  stopping: msg('Bot 正在停止，停完後再送'),
}

export function occurrenceLine(o: { status: OccurrenceStatus; waiting_reason?: string | null; turn_status?: string | null; turn_awaits_start?: number | null; turn_start_error?: string | null; next_retry_at?: string | null; attempt_no?: number }, liveTurnStatus?: string): string {
  if (o.status === 'pending' && o.next_retry_at) return t('等待重試（第 {n} 次執行）', { n: o.attempt_no ?? 1 })
  if (o.status === 'pending') return o.waiting_reason ? tr(WAIT_LABEL[o.waiting_reason] ?? msg('等待中')) : t('等待送出')
  if (o.status === 'queued') {
    const st = liveTurnStatus ?? o.turn_status
    if (st === 'in_flight') return t('執行中')
    if (st === 'completed' || st === 'completed_fallback' || st === 'failed') return t('回合已結束，結果同步中…')
    if (st === 'queued' && o.turn_awaits_start === 1 && o.turn_start_error) return t('Bot 啟動失敗（尚未送出）：{reason}', { reason: o.turn_start_error })
    return t('已排隊，等待 Bot 接收')
  }
  return tr(OCCURRENCE_LABEL[o.status])
}

export function scheduleStatusLine(s: Schedule, liveTurnStatus?: string): string {
  if (s.enabled === false) return t('已停用')
  if (s.open) return occurrenceLine(s.open, liveTurnStatus)
  if (!s.next_due) return t('已完成')
  const range = randomSummary(s.spec)
  const next = t('下次 {due}', { due: formatDue(s.next_due, s.tz, range !== '') })
  return range ? `${range} · ${next}` : next
}

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

export function scheduleStatusShort(s: Schedule, liveTurnStatus?: string): string {
  if (s.enabled === false) return t('已停用')
  if (s.open) return occurrenceLine(s.open, liveTurnStatus)
  if (!s.next_due) return t('已完成')
  const range = randomSummary(s.spec)
  return range ? `${range} · ${formatDueShort(s.next_due, s.tz, true)}` : formatDueShort(s.next_due, s.tz)
}

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

const ZH_WEEKDAY = ['一', '二', '三', '四', '五', '六', '日']
const EN_WEEKDAY = ['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']

function zoneOf(tz: string): string {
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: tz })
    return tz
  } catch {
    return 'UTC'
  }
}

interface ZonedParts { y: number; m: number; d: number; hm: string; wd: number }

function zonedParts(d: Date, zone: string): ZonedParts {
  const p = new Intl.DateTimeFormat('en-US', { timeZone: zone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hourCycle: 'h23', weekday: 'short' }).formatToParts(d)
  const v = (type: string) => p.find((x) => x.type === type)?.value ?? ''
  return { y: Number(v('year')), m: Number(v('month')), d: Number(v('day')), hm: `${v('hour')}:${v('minute')}`, wd: EN_WEEKDAY.indexOf(v('weekday')) }
}

function zoneTag(d: Date, zone: string): string {
  if (zone === defaultTimeZone()) return ''
  const part = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'short' }).formatToParts(d).find((x) => x.type === 'timeZoneName')
  return part?.value ?? zone
}

function weekdayName(i: number): string {
  return getLang() === 'en' ? EN_WEEKDAY[i] : ZH_WEEKDAY[i]
}

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

function openRunning(o: ScheduleOccurrence, liveTurnStatus: string | undefined): boolean {
  return !!o.running_since || (liveTurnStatus ?? o.turn_status) === 'in_flight'
}

function openText(o: ScheduleOccurrence, liveTurnStatus: string | undefined, nowMs: number): string {
  if (openRunning(o, liveTurnStatus)) {
    const ms = totalDurationMs({ duration_ms: o.duration_ms ?? 0, running_since: o.running_since }, nowMs) ?? 0
    return t('執行中 · 已 {d}', { d: formatDuration(ms) })
  }
  if (o.status === 'pending' && o.waiting_reason === 'bot_busy') return t('等待 Bot（忙碌中）')
  if (o.status === 'pending' && o.waiting_reason === 'base_building') return t('等待基準快照建立')
  return occurrenceLine(o, liveTurnStatus)
}

export interface RowLine { label?: string; text: string; title: string; tone?: 'needs-rebind' }
export type RowDot = 'ok' | 'fail' | 'never' | 'off' | 'waiting'

export function scheduleRowLines(s: Schedule, liveTurnStatus: string | undefined, nowMs: number): { dot: RowDot; next: RowLine; last: RowLine } {
  const now = new Date(nowMs)
  const random = randomRanges(s.spec)
  const rangeText = random.map((r) => `${clockHm(r.time)}–${clockHm(r.until)}`).join('、')
  let next: RowLine
  if (s.bot_available === false) {
    next = { text: t('⚠ 需要重新指定 Bot'), title: t('⚠ 需要重新指定 Bot'), tone: 'needs-rebind' }
  } else if (s.open) {
    const base = openText(s.open, liveTurnStatus, nowMs)
    const text = s.enabled === false ? `${base} · ${t('已停用')}` : base
    next = { label: t('現在'), text, title: text }
  } else if (s.enabled === false) {
    const c = cadenceText(s)
    next = { text: `${t('已停用')} · ${t('原訂{c}', { c })}`, title: `${t('已停用')} · ${t('原訂{c}', { c })}` }
  } else if (!s.next_due) {
    next = { text: t('已完成，沒有下一次'), title: t('已完成，沒有下一次') }
  } else {
    const when = whenText(s.next_due, s.tz, now)
    const text = random.length ? `${when}${t('（隨機）')}` : when
    const tip = [formatZoned(s.next_due, s.tz, random.length > 0), random.length ? t('隨機區間 {r}', { r: rangeText }) : ''].filter(Boolean).join(' · ')
    next = { label: t('下次'), text, title: tip }
  }

  return { dot: rowDot(s, liveTurnStatus), next, last: lastLine(s, now) }
}

function scheduleLastOk(s: Schedule): boolean {
  const o = s.last
  return !!o && (o.outcome === 'success' || (o.outcome == null && o.status === 'done'))
}

function rowDot(s: Schedule, liveTurnStatus: string | undefined): RowDot {
  if (s.enabled === false && !s.open) return 'off'
  if (s.open && !openRunning(s.open, liveTurnStatus)) return 'waiting'
  if (!s.last || s.last.status === 'cancelled') return 'never'
  return scheduleLastOk(s) ? 'ok' : 'fail'
}

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
