/**
 * codex 的方案沒有 5 小時窗（ChatGPT Pro：app-server 只回一個 10080 分鐘的週窗，使用者 2026-10-12）。
 *
 * 判斷只靠快照本身，不寫死方案名：有週窗、而 5 小時窗不存在——或只是 daemon 沿用下來的舊讀數
 * （`observed_at` 比 `updated_at` 舊，而週窗是這一次真的帶進來的）。plus 的 app-server 讀數兩個窗一起來，
 * 不會出現「週窗是新的、5h 是沿用的」，所以這個形狀就是「這個帳號已經沒有 5h 上限」。
 * 其他 kind 一律不適用（claude 的 statusLine 本來就會缺桶，那是 #540 的「舊」記號）。
 */
import type { BotKind, KindQuota } from '../api/types'
import { carriedOverAt } from './quotaWindowAge'

export function noFiveHourCap(kind: BotKind, q: KindQuota | null | undefined): boolean {
  if (kind !== 'codex' || !q?.seven_day) return false
  if (!q.five_hour) return true
  return carriedOverAt(q.five_hour, q.updated_at) !== null && carriedOverAt(q.seven_day, q.updated_at) === null
}

/**
 * 沒有 5h 上限時給畫面用的快照：拿掉沿用下來的 5h；撞上限的橫幅若是在這份讀數**之前**記的、
 * 而週窗還有剩，那是升級前 5h 用完留下的，也拿掉（credits／週窗真的用完時 daemon 會記新的）。
 */
export function projectNoFiveHour(kind: BotKind, q: KindQuota | null): KindQuota | null {
  if (!q || !noFiveHourCap(kind, q)) return q
  const hitAt = Date.parse(q.limit_hit?.at ?? '')
  const seenAt = Date.parse(q.seven_day?.observed_at ?? q.updated_at)
  const weekLeft = 100 - (q.seven_day?.used_pct ?? 100)
  const staleHit = q.limit_hit != null && weekLeft > 0 && !Number.isNaN(hitAt) && !Number.isNaN(seenAt) && hitAt < seenAt
  if (!q.five_hour && !staleHit) return q
  return { ...q, five_hour: null, limit_hit: staleHit ? null : q.limit_hit }
}
