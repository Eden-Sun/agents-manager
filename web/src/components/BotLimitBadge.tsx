import { useNow } from '../hooks/useHostOffline'
import type { BotLimitHit } from '../api/types'
import { limitHitWhen } from '../lib/limitHitWhen'

/**
 * 主力 bot 撞限（#1899）：daemon 的 `bot.limit_hit` 有值就掛「到限」晶片，過了 `until` 自動消失。
 * 判斷全在 daemon（`runners/quota.rs::try_limit_hit_for_bot`），這裡只比時間；沒寫 `until` 的照掛，等 daemon 清掉。
 */
export function BotLimitBadge({ hit, who }: { hit: BotLimitHit | null | undefined; who: string }) {
  const now = useNow(Boolean(hit))
  if (!hit) return null
  const until = hit.until ? Date.parse(hit.until) : NaN
  if (!Number.isNaN(until) && until <= now) return null
  const when = limitHitWhen(hit.until)
  const title = `${who} 已達用量上限${when ? `，${when} 恢復` : '，恢復時間 CLI 沒寫'}`
  return (
    <span className="bot-limit-hit" title={`${title}\n${hit.message}`}>
      到限
    </span>
  )
}
