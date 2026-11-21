import { balanceView } from '../api/claudeApiBalance'
import { useClaudeApiBalance } from '../hooks/useClaudeApiBalance'
import { useOpenAiApiBalance } from '../hooks/useOpenAiApiBalance'
import { useUserTimeZone } from '../lib/userTime'

export function useClaudeBalanceView() {
  const tz = useUserTimeZone()
  const balance = useClaudeApiBalance()
  return { balance, view: balanceView(balance, new Date(), tz) }
}

export function useClaudeBalanceVisible(): boolean {
  return useClaudeBalanceView().view.usd !== undefined
}

export function useOpenAiBalanceVisible(): boolean {
  return Boolean(useOpenAiApiBalance()?.enabled)
}
