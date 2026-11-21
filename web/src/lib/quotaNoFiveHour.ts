import type { BotKind, KindQuota } from '../api/types'
import { carriedOverAt } from './quotaWindowAge'

export function noFiveHourCap(kind: BotKind, q: KindQuota | null | undefined): boolean {
  if (kind !== 'codex' || !q?.seven_day) return false
  if (!q.five_hour) return true
  return carriedOverAt(q.five_hour, q.updated_at) !== null && carriedOverAt(q.seven_day, q.updated_at) === null
}

export function projectNoFiveHour(kind: BotKind, q: KindQuota | null): KindQuota | null {
  if (!q || !noFiveHourCap(kind, q)) return q
  const hitAt = Date.parse(q.limit_hit?.at ?? '')
  const seenAt = Date.parse(q.seven_day?.observed_at ?? q.updated_at)
  const weekLeft = 100 - (q.seven_day?.used_pct ?? 100)
  const staleHit = q.limit_hit != null && weekLeft > 0 && !Number.isNaN(hitAt) && !Number.isNaN(seenAt) && hitAt < seenAt
  if (!q.five_hour && !staleHit) return q
  return { ...q, five_hour: null, limit_hit: staleHit ? null : q.limit_hit }
}
