import { useCacheInput } from '../hooks/useCacheInput'
import { cacheLabel, cacheView } from '../lib/composerCost'
import { useT } from '../lib/i18n'

/**
 * bot 列 `⋯` 選單頂端的快取倒數（2026-11-02 使用者：child 沒有聊天區狀態列，menu 裡也要看得到）。
 * 只在選單打開時掛載，不讓每一列都跟著 15 秒 tick；codex 必快取、沒有倒數，不顯示。
 */
export function BotRowCacheNote({ botId }: { botId: string }) {
  const t = useT()
  const view = cacheView(useCacheInput(botId))
  if (!view) return null
  return (
    <div className="head-menu-note" role="none" title={view.approx ? t('依 codex 的 rollout 推估，實際以 API 為準') : undefined}>
      {t('快取')} {cacheLabel(view)}
    </div>
  )
}
