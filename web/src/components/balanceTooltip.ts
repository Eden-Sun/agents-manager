import type { useT } from '../lib/i18n'
import type { BalanceView, ClaudeApiBalance } from '../api/claudeApiBalance'

type Translate = ReturnType<typeof useT>

const hhmm = (iso: string | null | undefined): string => {
  const d = iso ? new Date(iso) : null
  if (!d || Number.isNaN(d.getTime())) return '--:--'
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

/** Balance 晶片 tooltip 的逐行文字。`t` 由元件的 `useT()` 傳進來（模組層不呼叫 msg()）。 */
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
    lines.push(t('錨點：${usd}（{at}）', { usd: balance.anchor.usd.toFixed(2), at: balance.anchor.at }))
    if (balance.spent_usd !== undefined) lines.push(t('錨點以來花費：${spent}', { spent: balance.spent_usd.toFixed(4) }))
    if (balance.report_through) lines.push(t('官方報表涵蓋至：{dt}', { dt: balance.report_through }))
    if (balance.local_since) lines.push(t('本機帳計算自：{dt}', { dt: balance.local_since }))
    if (balance.error) lines.push(t('警告：{err}', { err: balance.error }))
  }
  return lines
}
