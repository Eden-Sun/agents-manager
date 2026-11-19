import { BalanceQuotaChip } from './BalanceQuotaChip'
import { useClaudeBalanceVisible, useOpenAiBalanceVisible } from './balanceHooks'
import { OpenAiBalanceChip } from './OpenAiBalanceChip'

/**
 * 額度列裡的「API 餘額」群組（使用者 2026-11-19 截圖）：Claude 與 OpenAI 上下兩行，跟量表同排，但放在 `.quota-open` **外面**，
 * 不會被收合裁掉（之前放在最後一格，空間不夠時最先被裁的是 OpenAI）。寬度由 `QuotaStrip` 的 fit 量測先扣掉它。
 * 只有一家有值就一行；兩家都不可見就整個不畫。
 */
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
