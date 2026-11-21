import { BalanceQuotaChip } from './BalanceQuotaChip'
import { useClaudeBalanceVisible, useOpenAiBalanceVisible } from './balanceHooks'
import { OpenAiBalanceChip } from './OpenAiBalanceChip'

export function ApiBalanceGroup() {
  const claude = useClaudeBalanceVisible()
  const openai = useOpenAiBalanceVisible()
  if (!claude && !openai) return null
  return (
    <div className="quota-api-group" data-k="quota-api-group">
      {claude ? <BalanceQuotaChip /> : null}
      {openai ? <OpenAiBalanceChip /> : null}
    </div>
  )
}
