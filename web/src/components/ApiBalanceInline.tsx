import { useEffect, useState } from 'react'
import { balanceView, fetchClaudeApiBalance, type ClaudeApiBalance } from '../api/claudeApiBalance'
import { balanceTooltipLines } from './balanceTooltip'
import { useT } from '../lib/i18n'
import './apiBotUsageBar.css'
import { useUserTimeZone } from '../lib/userTime'

export function ApiBalanceInline() {
  const t = useT()
  const [balance, setBalance] = useState<ClaudeApiBalance | null>(null)
  useEffect(() => {
    let alive = true
    const load = () => {
      fetchClaudeApiBalance().then((b) => { if (alive) setBalance(b) }).catch(() => {})
    }
    load()
    const id = setInterval(load, 60_000)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [])
  const tz = useUserTimeZone()
  const bal = balanceView(balance, new Date(), tz)
  if (bal.usd === undefined && !bal.loggedOut) return null
  const title = bal.source === 'console' || bal.loggedOut ? balanceTooltipLines(balance, bal, t).join('\n') : t('Anthropic 預付餘額（估計）')
  return (
    <span className={`mobile-bot-balance${bal.expiringSoon || bal.loggedOut ? ' warn' : ''}`} title={title}>
      {bal.usd === undefined ? '—' : `${bal.source === 'console' ? '' : '≈ '}$${bal.usd.toFixed(2)}`}
      {bal.loggedOut ? ' ⚠' : ''}
    </span>
  )
}
