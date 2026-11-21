
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
import { useT } from '../lib/i18n'
import './mobilePreview.css'

export function MobilePreviewButton() {
  const t = useT()
  const open = useMobilePreviewOpen()
  const drawer = useMediaQuery(DRAWER_QUERY)
  if (IN_MOBILE_PREVIEW || drawer) return null
  return (
    <button
      type="button"
      className={`icon-btn mp-button icon-tip${open ? ' on' : ''}`}
      aria-pressed={open}
      aria-label={open ? t('關閉手機預覽') : t('開啟手機預覽')}
      data-tip={open ? t('關閉 · 手機預覽') : t('手機預覽 · {size}', { size: `${MOBILE_PREVIEW_W}×${MOBILE_PREVIEW_H}` })}
      onClick={() => setMobilePreviewOpen(!open)}
    >
      <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true">
        <rect x="4.25" y="1.75" width="7.5" height="12.5" rx="1.6" fill="none" stroke="currentColor" strokeWidth="1.4" />
        <path d="M7 11.75h2" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      </svg>
    </button>
  )
}

export function MobilePreview() {
  const t = useT()
  const open = useMobilePreviewOpen()
  const drawer = useMediaQuery(DRAWER_QUERY)
  const [nonce, setNonce] = useState(0)

  const reload = useCallback(() => setNonce((n) => n + 1), [])

  if (!open || drawer) return null

  const src = `${window.location.pathname}?mobilePreview=1`

  return (
    <aside className="mobile-preview" aria-label={t('手機預覽')}>
      <div className="mp-head">
        <span className="mp-title">{t('手機預覽')}</span>
        {}
        <span className="mp-size">
          {MOBILE_PREVIEW_W}×{MOBILE_PREVIEW_H} · {Math.round(MOBILE_PREVIEW_SCALE * 100)}%
        </span>
        <button type="button" className="icon-btn" title={t('重新載入預覽')} aria-label={t('重新載入預覽')} onClick={reload}>
          ↻
        </button>
        <button
          type="button"
          className="icon-btn"
          title={t('關閉手機預覽（右欄標題列的手機圖示可以重新開啟）')}
          aria-label={t('關閉手機預覽')}
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
          title={t('手機預覽')}
          src={src}
          sandbox="allow-same-origin allow-scripts allow-forms"
        />
        </div>
      </div>
    </aside>
  )
}
