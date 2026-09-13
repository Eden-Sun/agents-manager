/**
 * 標題列上的「● 需要回應」chip：agent `blocked` 時常駐，任何分頁都看得到，點下去開 `BlockedModal`。
 * 2026-09-12 使用者：人在終端分頁時只剩一顆 8px 紅燈。沒有「知道了」——沒回答就不該關得掉。
 */
import { useStore } from '../store/store'
import './blockedBadge.css'

export function BlockedBadge({ botId, onOpen }: { botId: string; onOpen: () => void }) {
  const blocked = useStore((s) => s.runs[botId]?.agent_status === 'blocked')
  if (!blocked) return null
  return (
    <button
      type="button"
      className="blocked-badge"
      title="agent 停在一個要你回答的提示上，點一下開全畫面終端回答"
      onClick={onOpen}
    >
      <span className="blocked-badge-dot" aria-hidden="true" />
      需要回應
    </button>
  )
}
