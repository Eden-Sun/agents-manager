import { useEffect, useState } from 'react'
import { fetchClaudeApiBalance, type ClaudeApiBalance } from '../api/claudeApiBalance'
import { useT } from '../lib/i18n'
import './apiBotUsageBar.css'

/** 手機標題第二行的 claude-api 預付餘額（#1944）：桌面在工具列的 Balance 格，手機那條收在彈窗裡看不到。沒設定就不畫。 */
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
  if (!balance?.configured || balance.remaining_usd === undefined) return null
  return (
    <span className="mobile-bot-balance" title={t('Anthropic 預付餘額（估計）')}>
      ≈ ${balance.remaining_usd.toFixed(2)}
    </span>
  )
}
