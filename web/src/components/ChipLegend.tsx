import { useEffect } from 'react'
import { createPortal } from 'react-dom'
import { StatusLamp } from './StatusLamp'
import './chipLegend.css'

/**
 * 主力晶片的顏色說明（2026-10-04 使用者：「手機版主力長按說明顏色意義；電腦版你自己想」）。
 * 範例直接套晶片與燈號的真 class，顏色永遠跟晶片列一致，不另外寫一份色票。
 * 手機：主力晶片長按不移動、放開就開（長按後移動仍是拖曳排序）；電腦：晶片列尾端的「?」。
 */
export function ChipLegend({ onClose, hover = false }: { onClose: () => void; /** 電腦版：滑鼠停在「?」上顯示，不蓋底、不吃點擊，移開就收。 */ hover?: boolean }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  return createPortal(
    <div className={`chip-legend-backdrop${hover ? ' hover' : ''}`} onClick={onClose}>
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
            <span>左側深色條＋粗體：你正在看的這顆</span>
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
            <StatusLamp lamp="idle" background={1} title="背景執行中" />
            <span>綠點外圈轉藍弧：回合結束，背景還在跑</span>
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
        <p className="chip-legend-tip">手機：長按晶片後拖曳可以排順序；長按不動放開就是這份說明。</p>
      </div>
    </div>,
    document.body,
  )
}
