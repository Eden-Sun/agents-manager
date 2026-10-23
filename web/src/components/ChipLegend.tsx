import { useEffect, useRef } from 'react'
import { createPortal } from 'react-dom'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { StatusLamp } from './StatusLamp'
import './chipLegend.css'
import './keepWarmChip.css'
import { FrozenBadge } from './FrozenBadge'
import './modelCapacity.css'
import { useT } from '../lib/i18n'

/**
 * 主力晶片的顏色說明（2026-10-04 使用者：「手機版主力長按說明顏色意義；電腦版你自己想」）。
 * 範例直接套晶片與燈號的真 class，顏色永遠跟晶片列一致，不另外寫一份色票。
 * 從主力 bot 狀態卡（`BotStatusCard`，手機長按）的「顏色代表什麼？」或電腦版晶片列尾端的「?」打開。
 */
export function ChipLegend({ onClose }: { onClose: () => void }) {
  const t = useT()
  const dialogRef = useRef<HTMLDivElement>(null)
  useDialogFocus(true, dialogRef)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  return createPortal(
    <div className="chip-legend-backdrop" onClick={onClose}>
      <div ref={dialogRef} className="chip-legend" role="dialog" aria-modal="true" aria-label={t('主力晶片的顏色說明')} onClick={(e) => e.stopPropagation()}>
        <div className="chip-legend-head">
          <strong>{t('顏色代表什麼')}</strong>
          <button type="button" className="chip-legend-close" aria-label={t('關閉')} onClick={onClose}>
            ✕
          </button>
        </div>
        <ul className="chip-legend-list">
          <li>
            <span className="unread-chip needs-reply" aria-hidden="true">
              <span className="unread-chip-dot" />
              <span className="unread-chip-name">bot</span>
            </span>
            <span>{t('停在等你回答（問題、權限框）——最急')}</span>
          </li>
          <li>
            <span className="unread-chip at-capacity" aria-hidden="true">
              <span className="unread-chip-dot" />
              <span className="unread-chip-name">bot</span>
            </span>
            <span>{t('紫色：模型滿載（OpenAI 容量不夠，這一回合沒跑）——不是卡住、也不是額度用完；換模型或稍後再送')}</span>
          </li>
          <li>
            <span className="unread-chip unread" aria-hidden="true">
              <span className="unread-chip-name">bot</span>
              <span className="unread-chip-n">2</span>
            </span>
            <span>{t('藍色數字：做完了還沒看；數字是幾個回合')}</span>
          </li>
          <li>
            <span className="unread-chip pinned keep-warm-replied" aria-hidden="true">
              <span className="unread-chip-name">bot</span>
              <span className="unread-chip-warm">♨︎</span>
            </span>
            <span>{t('♨ 標記：保溫回覆到了（cache 還熱），送出新 prompt 才恢復；不算未讀')}</span>
          </li>
          <li>
            <span className="unread-chip pinned" aria-hidden="true">
              <span className="unread-chip-name">bot</span>
              <FrozenBadge />
            </span>
            <span>{t('雪花：「不用保溫」（冰凍）；這一輪閒置不保溫，context 夠大時 cache 到期前自動壓縮一次；有新活動就恢復')}</span>
          </li>
          <li>
            <span className="unread-chip waits-kids" aria-hidden="true">
              <span className="unread-chip-working-ring" />
              <span className="unread-chip-name">bot</span>
            </span>
            <span>{t('黃色旋轉外框＋分叉圖示：child 還在跑，父 Bot 等它完成')}</span>
          </li>
          <li>
            <span className="unread-chip working" aria-hidden="true">
              <span className="unread-chip-working-ring" />
              <span className="unread-chip-name">bot</span>
            </span>
            <span>{t('黃色旋轉的整圈外框：還在執行')}</span>
          </li>
          <li>
            <span className="unread-chip current" aria-hidden="true">
              <span className="unread-chip-name">bot</span>
            </span>
            <span>{t('藍框＋粗體：你正在看的這顆（其他記號照它的狀態，例如藍色數字就是做完了還沒看）')}</span>
          </li>
          <li>
            <span className="unread-chip pin-more needs-reply" aria-hidden="true">
              <span className="unread-chip-name">+3</span>
            </span>
            <span>{t('收起來的；顏色跟著裡面最急的那顆')}</span>
          </li>
        </ul>
        <div className="chip-legend-sub">{t('燈號')}</div>
        <ul className="chip-legend-list lamps">
          <li>
            <StatusLamp lamp="working" title={t('執行中')} />
            <span>{t('藍、會擴散：執行中')}</span>
          </li>
          <li>
            <StatusLamp lamp="idle" kids={1} title={t('子 agent 還在跑')} />
            <span>{t('綠點外圈轉藍弧：它閒著，但底下的子 agent 還在跑（只有背景 shell 不轉，旁邊寫「背景 N」）')}</span>
          </li>
          <li>
            <StatusLamp lamp="blocked" title={t('等你回答')} />
            <span>{t('紅：卡在要你處理的畫面')}</span>
          </li>
          <li>
            <StatusLamp lamp="disconnected" title={t('斷線')} />
            <span>{t('灰：主機斷線、讀不到')}</span>
          </li>
        </ul>
        <p className="chip-legend-tip">{t('手機：長按晶片會跳出它的狀態卡，手指不放直接拖就能排順序；主力的狀態卡裡可以設定「不用保溫」。')}</p>
      </div>
    </div>,
    document.body,
  )
}
