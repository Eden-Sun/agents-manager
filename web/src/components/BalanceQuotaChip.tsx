import { balanceView } from '../api/claudeApiBalance'
import { useClaudeApiBalance } from '../hooks/useClaudeApiBalance'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useT } from '../lib/i18n'
import { useStore } from '../store/store'
import { balanceTooltipLines } from './balanceTooltip'
import './balanceQuotaChip.css'

const hhmm = (iso: string | null | undefined): string => {
  const d = iso ? new Date(iso) : null
  if (!d || Number.isNaN(d.getTime())) return '--:--'
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

/**
 * 全域額度列的 Anthropic API 餘額（使用者 2026-11-15：「額度列找個位置放餘額」）。
 * 數字：Console 讀得到就是 Console 實際值（`API $23.29`），否則是用同一套公式的**估算**（`API ≈$23.29`：錨點－官方報表－本機已確定花費，
 * 預留與結果不明的不扣、列在 tooltip）。手機縮成短格式 `$23.3`。沒設定（沒有錨點也讀不到 Console）就整格不畫。
 * 點下去切到第一顆 claude-api bot（狀態列有上限、每回合設定與餘額錨點可改）；沒有 claude-api bot 時只是顯示、不可點。
 */
export function BalanceQuotaChip() {
  const t = useT()
  const phone = useMediaQuery(PHONE_QUERY)
  const balance = useClaudeApiBalance()
  const apiBotId = useStore((s) => s.bots.find((b) => b.kind === 'claude-api')?.id ?? null)
  const selectBot = useStore((s) => s.selectBot)
  const view = balanceView(balance)
  if (view.usd === undefined) return null
  const exact = view.source === 'console'
  const usd = view.usd
  const text = phone ? `$${usd.toFixed(1)}` : `${exact ? '' : '≈'}$${usd.toFixed(2)}`
  const lines = balanceTooltipLines(balance, view, t)
  const stamp = exact ? balance?.console?.fetched_at : balance?.as_of
  const title = [
    exact ? t('Anthropic API 餘額（Console 實際值）') : t('Anthropic API 餘額（估算）'),
    ...lines,
    t('更新於 {time}', { time: hhmm(stamp) }),
    apiBotId ? t('點一下到 claude-api bot（上限與餘額錨點在它的狀態列）') : '',
  ]
    .filter(Boolean)
    .join('\n')
  const warn = view.loggedOut || view.expiringSoon || (balance?.uncertain_usd ?? 0) > 0
  const cls = `quota-balance${exact ? ' exact' : ' estimate'}${warn ? ' warn' : ''}`
  const body = (
    <>
      {phone ? null : <span className="quota-balance-k">API</span>}
      <span className="quota-balance-v">{text}</span>
    </>
  )
  return apiBotId ? (
    <button type="button" className={cls} data-k="quota-balance" title={title} onClick={() => selectBot(apiBotId)}>
      {body}
    </button>
  ) : (
    <span className={cls} data-k="quota-balance" title={title}>
      {body}
    </span>
  )
}
