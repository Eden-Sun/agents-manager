import { useWakeLock } from '../hooks/useWakeLock'
import { whyUnavailable } from '../lib/wakeLock'

/** 環境設定→顯示：手機盯著畫面等回話時不要鎖屏（2026-09-20 使用者）。預設關——一直亮著很耗電。 */
export function KeepAwakeToggle() {
  const { on, setOn, support, active } = useWakeLock()
  const usable = support === 'ok'
  return (
    <div className="keep-awake">
      <label className="keep-awake-row">
        <input type="checkbox" checked={on && usable} disabled={!usable} onChange={(e) => setOn(e.target.checked)} />
        <span>螢幕保持亮著（這台裝置）</span>
      </label>
      <p className="keep-awake-note">
        {!usable
          ? whyUnavailable(support)
          : on
            ? active
              ? '開著：這個分頁在前景時螢幕不會自己睡著；很耗電，不用時請關掉。'
              : '已開啟，但現在沒握住鎖（分頁在背景，或系統省電模式不給）。回到這個分頁會自動再拿一次。'
            : '關著：螢幕照系統設定自動鎖屏。'}
      </p>
    </div>
  )
}
