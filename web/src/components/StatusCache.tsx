/**
 * 聊天區上方狀態列的「快取」欄（接在 context 後面）：`快取 熱（剩 23 分）`／`快取 已冷`。
 * codex 沒有 statusLine，context 也在這裡由 rollout 補成同一段格式。markup 與 ChatPanel 的 `SlItem` 相同。
 */
import { CompactButton } from './CompactButton'
import { useCacheInput } from '../hooks/useCacheInput'
import { cacheLabel, cacheView, contextFallback, type CostStatus } from '../lib/composerCost'
import { t, useT } from '../lib/i18n'

export function StatusCache({ botId, status, contextAction = false }: { botId: string; status: CostStatus | null; contextAction?: boolean }) {
  useT()
  const input = useCacheInput(botId)
  // 狀態列的 status（codex 是推導出來的）才有沒有 context 的資訊；run.status 只有 claude 才有。
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
