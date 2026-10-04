/**
 * 聊天區上方狀態列的「快取」欄（接在 context 後面）：`快取 熱（剩 23 分）`／`快取 已冷`。
 * codex 沒有 statusLine，context 也在這裡由 rollout 補成同一段格式。markup 與 ChatPanel 的 `SlItem` 相同。
 */
import { useCacheInput } from '../hooks/useCacheInput'
import { cacheLabel, cacheView, contextFallback, type CostStatus } from '../lib/composerCost'

export function StatusCache({ botId, status }: { botId: string; status: CostStatus | null }) {
  const input = useCacheInput(botId)
  // 狀態列的 status（codex 是推導出來的）才有沒有 context 的資訊；run.status 只有 claude 才有。
  const ctx = contextFallback({ ...input, status })
  const view = cacheView(input)
  if (!ctx && !view) return null
  return (
    <>
      {ctx ? (
        <span className="sl-item" data-k="context">
          <span className="sl-k">context</span>
          <span className="sl-v">
            {ctx.pct}
            {ctx.detail ? <span className="sl-dim"> · {ctx.detail}</span> : null}
          </span>
        </span>
      ) : null}
      {view ? (
        <span className="sl-item" data-k="快取" title={view.approx ? '依 codex 的 rollout 推估，實際以 API 為準' : undefined}>
          <span className="sl-k">快取</span>
          <span className="sl-v">{cacheLabel(view)}</span>
        </span>
      ) : null}
    </>
  )
}
