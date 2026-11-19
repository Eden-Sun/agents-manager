import { balanceView } from '../api/claudeApiBalance'
import { useClaudeApiBalance } from '../hooks/useClaudeApiBalance'
import { useOpenAiApiBalance } from '../hooks/useOpenAiApiBalance'
import { useUserTimeZone } from '../lib/userTime'

/**
 * 全域 API 餘額的資料與可見性（晶片與額度選單的餘額區共用，不要各抄一份）。
 * 放在非元件檔：`fastRefreshExports` 規定 PascalCase 元件檔只能 export 元件。
 */
export function useClaudeBalanceView() {
  const tz = useUserTimeZone()
  const balance = useClaudeApiBalance()
  return { balance, view: balanceView(balance, new Date(), tz) }
}

/** 有錨點（或讀得到 Console）才可見；沒設定就整格不畫。 */
export function useClaudeBalanceVisible(): boolean {
  return useClaudeBalanceView().view.usd !== undefined
}

/** 有啟用才可見（未啟用整格不畫；啟用沒錨點仍畫「設定」入口）。 */
export function useOpenAiBalanceVisible(): boolean {
  return Boolean(useOpenAiApiBalance()?.enabled)
}
