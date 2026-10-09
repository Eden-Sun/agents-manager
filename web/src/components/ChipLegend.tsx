import { useEffect } from 'react'
import { createPortal } from 'react-dom'
import { StatusLamp } from './StatusLamp'
import './chipLegend.css'
import './keepWarmChip.css'

/**
 * 主力晶片的顏色說明（2026-10-04 使用者：「手機版主力長按說明顏色意義；電腦版你自己想」）。
 * 範例直接套晶片與燈號的真 class，顏色永遠跟晶片列一致，不另外寫一份色票。
 * 從主力 bot 狀態卡（`BotStatusCard`，手機長按）的「顏色代表什麼？」或電腦版晶片列尾端的「?」打開。
 */
export function ChipLegend({ onClose }: { onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  return createPortal(
    <div className="chip-legend-backdrop" onClick={onClose}>
      <div className="chip-legend" role="dialog" aria-label="主力晶片的顏色說明" onClick={(e) => e.stopPropagation()}>
        <div className="chip-legend-head">
          <strong>顏色代表什麼</strong>
          <button type="button" className="chip-legend-close" aria-label="關閉" onClick={onClose}>
            ✕
          </button>
        </div>
        <ul className="chip-legend-list">
          <li>
            <span className="unread-chip needs-reply" aria-hidden="true">
              <span className="unread-chip-dot" />
              <span className="unread-chip-name">bot</span>
            </span>
            <span>停在等你回答（問題、權限框）——最急</span>
          </li>
          <li>
            <span className="unread-chip unread" aria-hidden="true">
              <span className="unread-chip-name">bot</span>
              <span className="unread-chip-n">2</span>
            </span>
            <span>做完了還沒看；數字是幾個回合</span>
          </li>
          <li>
            <span className="unread-chip pinned keep-warm-replied" aria-hidden="true">
              <span className="unread-chip-name">bot</span>
              <span className="unread-chip-warm">♨︎</span>
            </span>
            <span>♨ 標記：保溫回覆到了（cache 還熱），送出新 prompt 才恢復；不算未讀</span>
          </li>
          <li>
            <span className="unread-chip waits-kids" aria-hidden="true">
              <span className="unread-chip-dot" />
              <span className="unread-chip-name">bot</span>
            </span>
            <span>黃點：在等子 agent，或子 agent 回報了還沒人看</span>
          </li>
          <li>
            <span className="unread-chip working" aria-hidden="true">
              <span className="unread-chip-dot" />
              <span className="unread-chip-name">bot</span>
            </span>
            <span>閃動的點：還在執行</span>
          </li>
          <li>
            <span className="unread-chip current" aria-hidden="true">
              <span className="unread-chip-name">bot</span>
            </span>
            <span>藍框＋粗體：你正在看的這顆</span>
          </li>
          <li>
            <span className="unread-chip pin-more needs-reply" aria-hidden="true">
              <span className="unread-chip-name">+3</span>
            </span>
            <span>收起來的；顏色跟著裡面最急的那顆</span>
          </li>
        </ul>
        <div className="chip-legend-sub">燈號</div>
        <ul className="chip-legend-list lamps">
          <li>
            <StatusLamp lamp="working" title="執行中" />
            <span>藍、會擴散：執行中</span>
          </li>
          <li>
            <StatusLamp lamp="idle" kids={1} title="子 agent 還在跑" />
            <span>綠點外圈轉藍弧：它閒著，但底下的子 agent 還在跑（只有背景 shell 不轉，旁邊寫「背景 N」）</span>
          </li>
          <li>
            <StatusLamp lamp="blocked" title="等你回答" />
            <span>紅：卡在要你處理的畫面</span>
          </li>
          <li>
            <StatusLamp lamp="disconnected" title="斷線" />
            <span>灰：主機斷線、讀不到</span>
          </li>
        </ul>
        <p className="chip-legend-tip">手機：長按晶片會跳出它的狀態卡，手指不放直接拖就能排順序。</p>
      </div>
    </div>,
    document.body,
  )
}
