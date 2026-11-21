import { CompactButton } from './CompactButton'
import { useCacheInput } from '../hooks/useCacheInput'
import { cacheLabel, cacheView, contextFallback, type CostStatus } from '../lib/composerCost'
import { t, useT } from '../lib/i18n'

export function StatusCache({ botId, status, contextAction = false }: { botId: string; status: CostStatus | null; contextAction?: boolean }) {
  useT()
  const input = useCacheInput(botId)
  const ctx = contextFallback({ ...input, status })
  const view = cacheView(input)
  const isCodex = input.kind === 'codex'
  if (!ctx && !view && !isCodex) return null
  return (
    <>
      {ctx || (isCodex && contextAction) ? (
        <span className="sl-item" data-k="context">
          <span className="sl-k">context</span>
          <span className="sl-v">{ctx ? <>{ctx.pct}{ctx.detail ? <span className="sl-dim"> · {ctx.detail}</span> : null}</> : '—'}{contextAction ? <CompactButton botId={botId} /> : null}</span>
        </span>
      ) : null}
      {isCodex || view ? (
        <span className="sl-item" data-k={t('快取')} title={view?.approx ? t('依 codex 的 rollout 推估，實際以 API 為準') : undefined}>
          <span className="sl-k">{isCodex ? 'codex' : t('快取')}</span>
          <span className="sl-v">{isCodex ? t('必快取') : cacheLabel(view!)}</span>
        </span>
      ) : null}
    </>
  )
}
