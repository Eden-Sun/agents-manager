import type { QuotaResetCredit, QuotaResetCredits } from '../api/types'
import { t } from './i18n'

/** 距到期不到這麼久就標警示色（2026-11-07 使用者）。 */
export const CREDIT_URGENT_MS = 48 * 3600_000
/** 到期前這麼久內提示使用者一次（2026-11-07 使用者：「前一天」）。 */
export const CREDIT_NOTICE_MS = 24 * 3600_000

export interface CreditRow {
  id: string
  title: string | null
  status: string
  expiresAt: string | null
  /** 距到期毫秒；沒有到期日或解不開＝null。 */
  left: number | null
}

/**
 * 重置券清單，依到期由早到晚（沒到期日的排最後）。舊 daemon 沒 `credits` 時回空陣列，呼叫端退回只看 `title`／`expires_at`。
 * 已經過期的不列（供應商偶爾晚一點才把券標成 expired）。
 */
export function creditRows(rc: QuotaResetCredits | null | undefined, now: number): CreditRow[] {
  const rows = (rc?.credits ?? []).map((c: QuotaResetCredit): CreditRow => {
    const at = c.expires_at ? Date.parse(c.expires_at) : NaN
    return { id: c.id, title: c.title, status: c.status, expiresAt: c.expires_at, left: Number.isNaN(at) ? null : at - now }
  })
  return rows
    .filter((r) => r.left === null || r.left > 0)
    .sort((a, b) => (a.left ?? Infinity) - (b.left ?? Infinity))
}

/** 最近一張要到期的券距到期毫秒（額度晶片上的小標記用）；沒有就 null。 */
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

/** `12/01 03:56`（瀏覽器時區）。 */
export function fmtCreditDate(iso: string | null | undefined): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '—'
  return d.toLocaleString([], { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false })
}

/** 倒數：`24 天後`／`5 小時後`／`不到 1 小時`。48 小時內改用小時，讓警示色那一格看得出急迫度。 */
export function fmtCreditIn(left: number): string {
  if (left <= 0) return t('已過期')
  const hours = left / 3600_000
  if (hours < 1) return t('不到 1 小時')
  if (hours < 48) return t('{n} 小時後', { n: Math.floor(hours) })
  return t('{n} 天後', { n: Math.floor(hours / 24) })
}
