/**
 * 輸入框上方的警示：平常完全不顯示，**只有快取已冷（且不在回合中）**才出一行警示色
 * 「快取已過期，送出這則會重寫約 811K」（UI-DECISIONS「快取狀態」）。只提示，不擋送出、不跳確認。
 */
import { apiColdWarning, cacheView, coldWarning } from '../lib/composerCost'
import { useCacheInput } from '../hooks/useCacheInput'
import { useStore } from '../store/store'
import './composerCostHint.css'

export function ComposerCostHint({ botId }: { botId: string }) {
  const input = useCacheInput(botId)
  const fast = useStore((s) => s.bots.find((b) => b.id === botId)?.fast ?? false)
  const view = cacheView(input)
  const text = input.kind === 'claude-api' ? apiColdWarning(view, input.model, input.ttlSecs, fast, input.apiLastTokens) : coldWarning(view)
  if (!text) return null
  return (
    <div className="composer-cost cold" role="status" title={text}>
      {text}
    </div>
  )
}
