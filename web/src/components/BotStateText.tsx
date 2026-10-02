import type { Lamp } from '../api/types'
import { LAMP_LABEL } from './lampLabel'

/**
 * 側欄 bot 列的狀態字。blocked 而且 daemon 說得出原因時（`run.blocked_reason`），狀態字旁多一行小字原因
 * （例如「codex 更新提示等待選擇」），hover 也有——使用者一眼知道要去 pane 處理什麼。沒有原因或不是 blocked 跟以前一樣。
 */
export function BotStateText({ lamp, reason }: { lamp: Lamp; reason: string }) {
  const why = lamp === 'blocked' ? reason : ''
  return (
    <span className={`bot-state ${lamp}`} title={why || undefined}>
      {LAMP_LABEL[lamp]}
      {why ? <span className="bot-state-reason" title={why}>{why}</span> : null}
    </span>
  )
}
