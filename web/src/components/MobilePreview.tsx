/**
 * 桌機右緣的手機預覽：在圖片暫存區底下嵌一個 390px 寬的 iframe，載入同一個 app。
 *
 * 為什麼要有：手機版的版面 bug（標題列折行、抽屜蓋住東西、bar 高度）只有在窄視窗才看得
 * 到，而開發時常態是寬視窗。以前要驗手機版得另開一個視窗再拉窄，或跑截圖腳本；現在把
 * 那一格直接掛在右欄，改完存檔 Vite 熱更新，左邊桌機版、右邊手機版一起動。
 *
 * 掛在圖片暫存區下面而不是浮在對話上，是因為右欄本來就是「工具欄」那一格（見
 * `docs/UI-DECISIONS.md`），浮動面板會蓋住時間軸與輸入框。
 *
 * 預設關閉、設定持久（localStorage，見 `store/mobilePreview.ts`）：這是 debug 工具，不該從一般
 * 使用者的對話寬度先扣
 * 一塊。開關在「環境設定 → 顯示」。
 *
 * 只在桌機出現：≤1024px 右欄已經變成底部那一條，塞不下 390px，而且那時候本來就是手機版。
 */

import { useCallback, useState } from 'react'
import { DRAWER_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import {
  IN_MOBILE_PREVIEW,
  MOBILE_PREVIEW_W,
  setMobilePreviewOpen,
  useMobilePreviewOpen,
} from '../store/mobilePreview'
import './mobilePreview.css'

/** 「環境設定 → 顯示」裡的那顆開關。 */
export function MobilePreviewToggle() {
  const open = useMobilePreviewOpen()
  const drawer = useMediaQuery(DRAWER_QUERY)
  if (IN_MOBILE_PREVIEW) return null
  return (
    <div className="mp-toggle">
      <span className="mp-toggle-label">手機預覽</span>
      <label className="mp-toggle-switch">
        <input type="checkbox" checked={open} onChange={(e) => setMobilePreviewOpen(e.target.checked)} />
        <span>在右欄圖片暫存區下方嵌一個 {MOBILE_PREVIEW_W}px 寬的預覽</span>
      </label>
      {drawer ? <p className="mp-toggle-note">目前視窗寬度已經是手機／平板版面，預覽不會顯示。</p> : null}
    </div>
  )
}

/** 右欄第二格。關著、或視窗已經窄到用手機版面時，整個不存在（不佔格子）。 */
export function MobilePreview() {
  const open = useMobilePreviewOpen()
  const drawer = useMediaQuery(DRAWER_QUERY)
  // 重新載入：換 key 讓 React 重建 iframe，比動 contentWindow.location 不會踩到同源以外的狀況。
  const [nonce, setNonce] = useState(0)

  const reload = useCallback(() => setNonce((n) => n + 1), [])

  if (!open || drawer) return null

  const src = `${window.location.pathname}?mobilePreview=1`

  return (
    <aside className="mobile-preview" aria-label="手機預覽">
      <div className="mp-head">
        <span className="mp-title">手機預覽</span>
        <span className="mp-size">{MOBILE_PREVIEW_W}px</span>
        <button type="button" className="icon-btn" title="重新載入預覽" aria-label="重新載入預覽" onClick={reload}>
          ↻
        </button>
        <button
          type="button"
          className="icon-btn"
          title="關閉手機預覽（可在環境設定 → 顯示 重新開啟）"
          aria-label="關閉手機預覽"
          onClick={() => setMobilePreviewOpen(false)}
        >
          ✕
        </button>
      </div>
      <div className="mp-frame-wrap">
        <iframe
          key={nonce}
          className="mp-frame"
          title="手機預覽"
          src={src}
          // 預覽是同源的自己：不需要額外權限，但也不該讓它彈出視窗或導走上層。
          sandbox="allow-same-origin allow-scripts allow-forms"
        />
      </div>
    </aside>
  )
}
