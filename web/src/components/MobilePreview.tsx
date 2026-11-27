
import { useCallback, useEffect, useRef, useState } from 'react'
import { DRAWER_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import {
  IN_MOBILE_PREVIEW,
  PREVIEW_DEVICES,
  previewScaleFor,
  previewHeightCapFor,
  setMobilePreviewDevice,
  setMobilePreviewOpen,
  useMobilePreviewDevice,
  useMobilePreviewOpen,
  type PreviewDevice,
} from '../store/mobilePreview'
import { useT } from '../lib/i18n'
import './mobilePreview.css'

export function MobilePreviewButton() {
  const t = useT()
  const open = useMobilePreviewOpen()
  const device = useMobilePreviewDevice()
  const drawer = useMediaQuery(DRAWER_QUERY)
  if (IN_MOBILE_PREVIEW || drawer) return null
  const { w, h } = PREVIEW_DEVICES[device]
  return (
    <button
      type="button"
      className={`icon-btn mp-button icon-tip${open ? ' on' : ''}`}
      aria-pressed={open}
      aria-label={open ? t('關閉手機預覽') : t('開啟手機預覽')}
      data-tip={open ? t('關閉 · 手機預覽') : t('手機預覽 · {size}', { size: `${w}×${h}` })}
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
  const device = useMobilePreviewDevice()
  const drawer = useMediaQuery(DRAWER_QUERY)
  const [nonce, setNonce] = useState(0)
  const wrapRef = useRef<HTMLDivElement>(null)
  const [wrapSize, setWrapSize] = useState<{ w: number; h: number }>({ w: 0, h: 0 })

  const reload = useCallback(() => setNonce((n) => n + 1), [])

  useEffect(() => {
    if (!open || drawer) return
    const el = wrapRef.current
    if (!el) return
    const measure = () => {
      const r = el.getBoundingClientRect()
      const w = Math.max(0, r.width - 16)
      const cap = previewHeightCapFor(device, window.innerHeight)
      const h = device === 'phone' ? Math.max(0, Math.min(cap, r.height > 16 ? r.height - 16 : cap)) : cap
      setWrapSize((prev) => (Math.abs(prev.w - w) < 1 && Math.abs(prev.h - h) < 1 ? prev : { w, h }))
    }
    measure()
    if (typeof ResizeObserver === 'undefined') {
      window.addEventListener('resize', measure)
      return () => window.removeEventListener('resize', measure)
    }
    const ro = new ResizeObserver(measure)
    ro.observe(el)
    window.addEventListener('resize', measure)
    return () => {
      ro.disconnect()
      window.removeEventListener('resize', measure)
    }
  }, [open, drawer, device])

  if (!open || drawer) return null

  const { w, h } = PREVIEW_DEVICES[device]
  const scale = previewScaleFor(device, wrapSize.w, wrapSize.h || undefined)
  const src = `${window.location.pathname}?mobilePreview=1`
  const choices: Array<{ id: PreviewDevice; label: string }> = [
    { id: 'phone', label: t('手機') },
    { id: 'ipad-portrait', label: t('iPad 直') },
    { id: 'ipad-landscape', label: t('iPad 橫') },
  ]

  return (
    <aside className="mobile-preview" data-device={device} aria-label={t('裝置預覽')}>
      <div className="mp-head">
        <span className="mp-title">{device === 'phone' ? t('手機預覽') : t('iPad 預覽')}</span>
        <span className="mp-size">
          {w}×{h} · {Math.round(scale * 100)}%
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
      <div className="mp-devices" role="group" aria-label={t('預覽裝置尺寸')}>
        {choices.map((c) => (
          <button
            key={c.id}
            type="button"
            className={`mp-dev-btn${device === c.id ? ' on' : ''}`}
            aria-pressed={device === c.id}
            onClick={() => setMobilePreviewDevice(c.id)}
          >
            {c.label}
          </button>
        ))}
      </div>
      <div ref={wrapRef} className="mp-frame-wrap">
        <div
          className="mp-frame-box"
          style={{ width: `${Math.round(w * scale)}px`, height: `${Math.round(h * scale)}px` }}
        >
          <iframe
            key={`${nonce}-${device}`}
            className="mp-frame"
            style={{ width: `${w}px`, height: `${h}px`, transform: `scale(${scale})` }}
            title={device === 'phone' ? t('手機預覽') : t('iPad 預覽')}
            src={src}
            sandbox="allow-same-origin allow-scripts allow-forms"
          />
        </div>
      </div>
    </aside>
  )
}
