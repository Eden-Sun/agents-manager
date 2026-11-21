import { apiColdWarning, cacheView, coldWarning } from '../lib/composerCost'
import { useCacheInput } from '../hooks/useCacheInput'
import { useStore } from '../store/store'
import { CompactButton } from './CompactButton'
import './composerCostHint.css'

export function ComposerCostHint({ botId }: { botId: string }) {
  const input = useCacheInput(botId)
  const fast = useStore((s) => s.bots.find((b) => b.id === botId)?.fast ?? false)
  const view = cacheView(input)
  const text = input.kind === 'openai-api' ? null : input.kind === 'claude-api' ? apiColdWarning(view, input.model, input.ttlSecs, fast, input.apiLastTokens) : coldWarning(view)
  if (!text) return null
  const showCompact = Boolean(view && view.state === 'hot' && view.mins !== null && view.mins <= 2)
  return (
    <div className="composer-cost cold" role="status" title={text}>
      <span>{text}</span>
      {showCompact ? <CompactButton botId={botId} /> : null}
    </div>
  )
}
