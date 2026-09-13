/**
 * 桌機右緣的手機預覽：在圖片暫存區底下嵌一個 iPhone 16（393×852）的 iframe，載入同一個 app，
 * 畫面上縮到 50%（2026-09-13 使用者：整格 406px 吃掉桌機太多寬度）。縮的是 `transform`，
 * iframe 自己的視窗仍然是 393×852，所以量到的版面就是手機上的那一份。
 *
 * 為什麼要有：手機版的版面 bug（標題列折行、抽屜蓋住東西、bar 高度）只有在窄視窗才看得
 * 到，而開發時常態是寬視窗。以前要驗手機版得另開一個視窗再拉窄，或跑截圖腳本；現在把
 * 那一格直接掛在右欄，改完存檔 Vite 熱更新，左邊桌機版、右邊手機版一起動。
 *
 * 掛在圖片暫存區下面而不是浮在對話上，是因為右欄本來就是「工具欄」那一格（見
 * `docs/UI-DECISIONS.md`），浮動面板會蓋住時間軸與輸入框。
 *
 * 預設關閉、設定持久（localStorage，見 `store/mobilePreview.ts`）：這是 debug 工具，不該從一般
 * 使用者的對話寬度先扣一塊。開關是右欄標題列上那顆手機圖示（2026-09-13 使用者：「手機預覽做在
 * 右邊 toggle 就好，不用在設定裡」）——要看的時候眼睛本來就在右欄，不必再繞進環境設定。
 *
 * 只在桌機出現：≤1024px 右欄已經變成底部那一條，塞不下 390px，而且那時候本來就是手機版。
 */

import { useCallback, useState } from 'react'
import { DRAWER_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import {
  IN_MOBILE_PREVIEW,
  MOBILE_PREVIEW_H,
  MOBILE_PREVIEW_SCALE,
  MOBILE_PREVIEW_W,
  setMobilePreviewOpen,
  useMobilePreviewOpen,
} from '../store/mobilePreview'
import './mobilePreview.css'

/** 右欄標題列上的手機預覽開關（圖片暫存那一列，⤢ 收合鍵左邊）。 */
export function MobilePreviewButton() {
  const open = useMobilePreviewOpen()
  const drawer = useMediaQuery(DRAWER_QUERY)
  // 預覽裡那份 app 不再長出巢狀預覽；視窗已經窄到手機／平板版面時，預覽本來就不會顯示。
  if (IN_MOBILE_PREVIEW || drawer) return null
  return (
    <button
      type="button"
      className={`icon-btn mp-button icon-tip${open ? ' on' : ''}`}
      aria-pressed={open}
      aria-label={open ? '關閉手機預覽' : '開啟手機預覽'}
      data-tip={open ? '關閉 · 手機預覽' : `手機預覽 · ${MOBILE_PREVIEW_W}×${MOBILE_PREVIEW_H}`}
      onClick={() => setMobilePreviewOpen(!open)}
    >
      <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true">
        <rect x="4.25" y="1.75" width="7.5" height="12.5" rx="1.6" fill="none" stroke="currentColor" strokeWidth="1.4" />
        <path d="M7 11.75h2" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      </svg>
    </button>
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
        {/* 寫出來的是 iframe 真正的視窗尺寸，不是它畫在畫面上的大小——量版面靠的是前者。 */}
        <span className="mp-size">
          {MOBILE_PREVIEW_W}×{MOBILE_PREVIEW_H} · {Math.round(MOBILE_PREVIEW_SCALE * 100)}%
        </span>
        <button type="button" className="icon-btn" title="重新載入預覽" aria-label="重新載入預覽" onClick={reload}>
          ↻
        </button>
        <button
          type="button"
          className="icon-btn"
          title="關閉手機預覽（右欄標題列的手機圖示可以重新開啟）"
          aria-label="關閉手機預覽"
          onClick={() => setMobilePreviewOpen(false)}
        >
          ✕
        </button>
      </div>
      <div className="mp-frame-wrap">
        {/* 盒子佔「縮放後」的格子，iframe 用 transform 縮進去（見 mobilePreview.css）。 */}
        <div className="mp-frame-box">
        <iframe
          key={nonce}
          className="mp-frame"
          title="手機預覽"
          src={src}
          // 預覽是同源的自己：不需要額外權限，但也不該讓它彈出視窗或導走上層。
          sandbox="allow-same-origin allow-scripts allow-forms"
        />
        </div>
      </div>
    </aside>
  )
}
