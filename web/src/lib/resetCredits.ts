import { fmtDateTime } from './userTime'
import type { QuotaResetCredit, QuotaResetCredits } from '../api/types'
import { t } from './i18n'

export const CREDIT_URGENT_MS = 48 * 3600_000
export const CREDIT_NOTICE_MS = 24 * 3600_000

export interface CreditRow {
  id: string
  title: string | null
  status: string
  expiresAt: string | null
  left: number | null
}

export function creditRows(rc: QuotaResetCredits | null | undefined, now: number): CreditRow[] {
  const rows = (rc?.credits ?? []).map((c: QuotaResetCredit): CreditRow => {
    const at = c.expires_at ? Date.parse(c.expires_at) : NaN
    return { id: c.id, title: c.title, status: c.status, expiresAt: c.expires_at, left: Number.isNaN(at) ? null : at - now }
  })
  return rows
    .filter((r) => r.left === null || r.left > 0)
    .sort((a, b) => (a.left ?? Infinity) - (b.left ?? Infinity))
}

export function nearestCreditLeft(rc: QuotaResetCredits | null | undefined, now: number): number | null {
  const fromList = creditRows(rc, now).find((r) => r.left !== null)?.left ?? null
  if (fromList !== null) return fromList
  if (!rc?.expires_at || (rc.credits?.length ?? 0) > 0 || rc.available <= 0) return null
  const at = Date.parse(rc.expires_at)
  return Number.isNaN(at) || at <= now ? null : at - now
}

export function creditUrgent(left: number | null): boolean {
  return left !== null && left < CREDIT_URGENT_MS
}

export function fmtCreditDate(iso: string | null | undefined): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '—'
  return fmtDateTime(d, { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
}

export function fmtCreditIn(left: number): string {
  if (left <= 0) return t('已過期')
  const hours = left / 3600_000
  if (hours < 1) return t('不到 1 小時')
  if (hours < 48) return t('{n} 小時後', { n: Math.floor(hours) })
  return t('{n} 天後', { n: Math.floor(hours / 24) })
}
