/**
 * 標題列上的「● 需要回應」chip：agent 進 `blocked`（在等使用者按 y/n、選選項…）時常駐。
 *
 * 為什麼要有它（2026-09-12 使用者：carbis 在等他回答，他人在「終端」分頁，畫面上只有一顆
 * 8px 的紅燈）：blocked 原本有三個出口——紅燈、自動彈的 `BlockedModal`（關掉一次就不再彈）、
 * 對話分頁上方的 `BlockedPanel`——後兩個都只在對話分頁；關掉全畫面又切到終端分頁，就只剩
 * 那顆燈。這顆 chip 跟名字同一列，桌機手機都畫、不管在哪個分頁，點下去直接開全畫面終端
 * （`BlockedModal`）回答；離開 blocked 就消失，沒有「知道了」——在等的事沒回答不該關得掉。
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
