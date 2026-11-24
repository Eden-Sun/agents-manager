import type { HostBaseline } from '../api/types'
import { t } from './i18n'
import { userTimeZone, zonedParts } from './userTime'

export type HostBaselineLevel = 'ok' | 'warn' | 'critical' | 'unknown' | 'stale'

export interface HostBaselineView {
  text: string
  level: HostBaselineLevel
  hint: string
  items: { id: string; severity: 'critical' | 'warn'; message: string; note?: string; fix: 'auto' | 'manual' | null; kind: 'write' | 'install' | 'manual' | null; command?: string }[]
}

const STALE_AFTER_MS = (6 * 60 + 15) * 60 * 1000

export function formatCheckedAt(iso: string, now: number = Date.now()): string {
  const ms = Date.parse(iso)
  if (Number.isNaN(ms)) return iso || t('不明')
  const p = (n: number) => String(n).padStart(2, '0')
  const zone = userTimeZone()
  const z = zonedParts(ms, zone)
  const year = z.year === zonedParts(now, zone).year ? '' : `${z.year}-`
  return `${year}${p(z.month)}-${p(z.day)} ${p(z.hour)}:${p(z.minute)}`
}

export function hostBaselineView(b: HostBaseline | null, now: number = Date.now()): HostBaselineView {
  if (!b) return { text: t('一致性：尚未檢查'), level: 'unknown', hint: t('還沒量過這台主機（剛連上、或 daemon 是舊版）'), items: [] }
  const checked = Date.parse(b.checked_at)
  const old = !Number.isNaN(checked) && now - checked > STALE_AFTER_MS
  const fixable = b.fixable === undefined ? null : new Set(b.fixable)
  const actions = new Map((b.actions ?? []).map((a) => [a.id, a]))
  const sorted = (b.issues === null ? [] : [...b.issues].sort((a, c) => Number(c.severity === 'critical') - Number(a.severity === 'critical'))).map((i) => ({
    ...i,
    fix: fixable === null ? null : fixable.has(i.id) ? ('auto' as const) : ('manual' as const),
    kind: actions.get(i.id)?.kind ?? null,
    command: actions.get(i.id)?.command,
  }))
  if (b.stale || old) {
    return {
      text: t('一致性：上次檢查於 {time}，目前無法連線', { time: formatCheckedAt(b.checked_at, now) }),
      level: 'stale',
      hint: b.error
        ? t('最後一次偵測失敗：{error}。下面是上次量到的結果，不是現況。', { error: b.error })
        : t('超過一個重量週期沒有更新（主機離線或偵測沒跑成）。下面是上次量到的結果，不是現況。'),
      items: sorted,
    }
  }
  if (b.issues === null) {
    return { text: t('一致性：未知'), level: 'unknown', hint: t('上一趟探測沒跑完（逾時或被截斷），這不代表缺東西；下一次偵測會再量'), items: [] }
  }
  if (b.issues.length === 0) return { text: t('一致性：與基準一致'), level: 'ok', hint: '', items: [] }
  const critical = sorted.filter((i) => i.severity === 'critical').length
  return {
    text: critical > 0 ? t('一致性：{n} 項不一致（{critical} 項嚴重）', { n: sorted.length, critical }) : t('一致性：{n} 項提醒', { n: sorted.length }),
    level: critical > 0 ? 'critical' : 'warn',
    hint: '',
    items: sorted,
  }
}
