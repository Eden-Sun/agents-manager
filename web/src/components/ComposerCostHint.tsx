/**
 * 輸入框上方的一行小字：目前 context 用量、prompt cache 是否還熱（UI-DECISIONS「輸入框上方的 context／快取提示」）。
 * 只提示，不擋送出、不跳確認；沒有任何可講的就整個不畫。
 */
import { useShallow } from 'zustand/react/shallow'
import { useStore, composerState } from '../store/store'
import { useCacheTick } from '../hooks/useCacheTick'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { composerCostHint } from '../lib/composerCost'
import './composerCostHint.css'

export function ComposerCostHint({ botId }: { botId: string }) {
  const run = useStore(
    useShallow((s) => {
      const r = s.runs[botId]
      return {
        kind: s.bots.find((b) => b.id === botId)?.kind ?? null,
        promptCache: r?.prompt_cache ?? null,
        status: r?.status ?? null,
        lastApiAt: r?.last_api_at ?? null,
        ttlSecs: r?.cache_ttl_secs ?? null,
        working: r?.agent_status === 'working' || composerState(s, botId).inFlightTurnId !== null,
      }
    }),
  )
  const phone = useMediaQuery(PHONE_QUERY)
  const now = useCacheTick(Boolean(run.ttlSecs))
  const hint = composerCostHint({ ...run, nowMs: now })
  if (!hint.context && !hint.cache) return null
  const cold = hint.cache?.kind === 'cold'
  const full = [hint.context, hint.cache?.text].filter(Boolean).join(' · ')
  return (
    <div className={`composer-cost${cold ? ' cold' : ''}`} role="status" title={full}>
      {hint.context ? <span className="composer-cost-ctx">{hint.context}</span> : null}
      {hint.cache ? <span className={`composer-cost-cache ${hint.cache.kind}`}>{phone ? hint.cache.short : hint.cache.text}</span> : null}
    </div>
  )
}
