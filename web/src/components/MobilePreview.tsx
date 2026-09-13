/**
 * 桌機右緣的 iPhone 16（393×852）預覽 iframe，用 transform 縮到 50%（2026-09-13 使用者：整格 406px 太寬）；iframe 視窗仍是 393×852。
 * 預設關閉、持久於 localStorage；開關在右欄標題列（2026-09-13 使用者：「手機預覽做在右邊 toggle 就好，不用在設定裡」）。≤1024px 不出現。
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

/** 右欄第二格；關著或已是手機版面時整個不存在。 */
export function MobilePreview() {
  const open = useMobilePreviewOpen()
  const drawer = useMediaQuery(DRAWER_QUERY)
  // 換 key 重建 iframe，避免動 contentWindow.location 碰到同源以外的狀況。
  const [nonce, setNonce] = useState(0)

  const reload = useCallback(() => setNonce((n) => n + 1), [])

  if (!open || drawer) return null

  const src = `${window.location.pathname}?mobilePreview=1`

  return (
    <aside className="mobile-preview" aria-label="手機預覽">
      <div className="mp-head">
        <span className="mp-title">手機預覽</span>
        {/* 顯示 iframe 真正的視窗尺寸，不是畫面上的大小。 */}
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
