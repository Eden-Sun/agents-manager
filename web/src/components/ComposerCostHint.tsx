/**
 * 輸入框上方的警示：平常完全不顯示，**只有快取已冷（且不在回合中）**才出一行警示色
 * 「快取已過期，送出這則會重寫約 811K」（UI-DECISIONS「快取狀態」）。只提示，不擋送出、不跳確認。
 */
import { cacheView, coldWarning } from '../lib/composerCost'
import { useCacheInput } from '../hooks/useCacheInput'
import './composerCostHint.css'

export function ComposerCostHint({ botId }: { botId: string }) {
  const text = coldWarning(cacheView(useCacheInput(botId)))
  if (!text) return null
  return (
    <div className="composer-cost cold" role="status" title={text}>
      {text}
    </div>
  )
}
