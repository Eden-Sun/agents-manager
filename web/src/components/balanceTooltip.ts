import type { useT } from '../lib/i18n'
import type { BalanceView, ClaudeApiBalance } from '../api/claudeApiBalance'

type Translate = ReturnType<typeof useT>

const hhmm = (iso: string | null | undefined): string => {
  const d = iso ? new Date(iso) : null
  if (!d || Number.isNaN(d.getTime())) return '--:--'
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

/** Balance 晶片 tooltip 的逐行文字。`t` 由元件的 `useT()` 傳進來（模組層不呼叫 msg()）。 */
/** 距上次校正多久：不到 1 小時／N 小時（兩天內）／N 天。 */
export function anchorAge(iso: string | null | undefined, now: Date, t: Translate): string {
  const ms = iso ? now.getTime() - new Date(iso).getTime() : Number.NaN
  if (!Number.isFinite(ms) || ms < 0) return '—'
  const h = Math.floor(ms / 3_600_000)
  if (h < 1) return t('不到 1 小時')
  if (h < 48) return t('{n} 小時', { n: h })
  return t('{n} 天', { n: Math.floor(h / 24) })
}

export function balanceTooltipLines(balance: ClaudeApiBalance | null | undefined, view: BalanceView, t: Translate): string[] {
  const c = balance?.console
  const lines: string[] = []
  if (view.source === 'console' && c) {
    lines.push(t('Console 實際（{time} 讀取）', { time: hhmm(c.fetched_at) }))
    lines.push(t('已購 ${purchased}＋促銷 ${promo}', { purchased: (c.purchased_usd ?? 0).toFixed(2), promo: (c.promotional_usd ?? 0).toFixed(2) }))
    for (const p of c.promotions) {
      lines.push(t('促銷 ${rem} / ${total}，到期 {date}', { rem: p.remaining_usd.toFixed(2), total: p.total_usd.toFixed(2), date: p.expires_on ?? '?' }))
    }
    if (view.expiringSoon && view.nextExpiry) {
      lines.push(t('⚠ 促銷額度 {date} 到期', { date: view.nextExpiry.expires_on ?? '?' }))
    }
    return lines
  }
  if (view.loggedOut) lines.push(t('Console 登入已過期，需重新匯出'))
  if (balance?.configured && balance.anchor) {
    lines.push(t('錨點：${usd}（{at}，距上次校正 {age}）', { usd: balance.anchor.usd.toFixed(2), at: balance.anchor.at, age: anchorAge(balance.anchor.at, new Date(), t) }))
    if (balance.spent_usd !== undefined) lines.push(t('錨點以來已確定花費：${spent}（官方報表＋本機已結算帳）', { spent: balance.spent_usd.toFixed(4) }))
    if (balance.uncertain_usd !== undefined && balance.uncertain_usd > 0) {
      lines.push(t('另有結果不明的請求最多 ${unk}（以預留上限計，實際多半低很多），最壞情況餘額 ${floor}', { unk: balance.uncertain_usd.toFixed(2), floor: (balance.remaining_floor_usd ?? 0).toFixed(2) }))
    }
    lines.push(t('這是估算：官方數字有數小時延遲、金額是折扣後但**扣 credits 之前**，儲值與贈送額度查不到；跟 Console 對不上時以 Console 為準，並點這裡用 Console 目前餘額校正'))
    if (balance.report_through) lines.push(balance.source === 'analytics' ? t('官方數字（小時粒度）涵蓋至：{dt}，之後用本機帳', { dt: balance.report_through }) : t('官方報表涵蓋至：{dt}', { dt: balance.report_through }))
    if (balance.local_since) lines.push(t('本機帳計算自：{dt}', { dt: balance.local_since }))
    if (balance.error) lines.push(t('警告：{err}', { err: balance.error }))
  }
  return lines
}
