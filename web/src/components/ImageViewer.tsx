import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { useT } from '../lib/i18n'
import './imageViewer.css'

export type Picture = { src: string; name: string }

export function ImageViewer({ pictures, index: initialIndex, onClose }: { pictures: Picture[]; index: number; onClose: () => void }) {
  const t = useT()
  const [index, setIndex] = useState(initialIndex)
  const [zoom, setZoom] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const viewport = useRef<HTMLDivElement>(null)
  const touch = useRef<{ x: number; y: number } | null>(null)
  const suppressClick = useRef(false)
  const close = useRef(onClose)
  useEffect(() => { close.current = onClose }, [onClose])
  const picture = pictures[index]
  const move = (delta: number) => {
    setIndex((i) => (i + delta + pictures.length) % pictures.length)
    setZoom(false)
    if (viewport.current) { viewport.current.scrollTop = 0; viewport.current.scrollLeft = 0 }
  }
  useDialogFocus(true, root)
  useEffect(() => {
    // Fixed body also locks Safari; retain the exact pre-viewer position/styles.
    const body = document.body
    const saved = { position: body.style.position, top: body.style.top, width: body.style.width, overflow: body.style.overflow }
    const x = window.scrollX, y = window.scrollY
    Object.assign(body.style, { position: 'fixed', top: `${-y}px`, width: '100%', overflow: 'hidden' })
    const key = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing) return
      if (event.key === 'Escape') { event.preventDefault(); close.current() }
      if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
        event.preventDefault()
        setIndex((i) => (i + (event.key === 'ArrowLeft' ? -1 : 1) + pictures.length) % pictures.length)
        setZoom(false)
      }
    }
    // App routeSync already handles browser Back by dispatching Escape to the top dialog.
    window.addEventListener('keydown', key)
    return () => {
      window.removeEventListener('keydown', key)
      Object.assign(body.style, saved)
      window.scrollTo(x, y)
    }
  }, [pictures.length])
  return createPortal(
    <div ref={root} className="image-viewer" role="dialog" aria-modal="true" aria-label={t('預覽 {name}', { name: picture.name })} tabIndex={-1}
      onClick={(event) => { event.stopPropagation(); if (event.target === event.currentTarget) onClose() }}>
      <header className="image-viewer-bar">
        <span className="image-viewer-name" title={picture.name}>{picture.name}</span>
        <span aria-live="polite">{index + 1} / {pictures.length}</span>
        <button type="button" onClick={onClose} aria-label={t('關閉')}>×</button>
      </header>
      <div ref={viewport} className={`image-viewer-stage${zoom ? ' zoomed' : ''}`}
        onClick={(event) => { if (event.target === event.currentTarget && !suppressClick.current) onClose() }}
        onTouchStart={(event) => {
          suppressClick.current = false
          touch.current = !zoom && event.touches.length === 1 ? { x: event.touches[0].clientX, y: event.touches[0].clientY } : null
        }}
        onTouchMove={(event) => { if (event.touches.length > 1) touch.current = null }}
        onTouchEnd={(event) => {
          const start = touch.current
          touch.current = null
          if (!start || !event.changedTouches.length) return
          const dx = event.changedTouches[0].clientX - start.x, dy = event.changedTouches[0].clientY - start.y
          if (Math.max(Math.abs(dx), Math.abs(dy)) < 50) return
          suppressClick.current = true
          if (dy > 70 && dy > Math.abs(dx)) onClose()
          else if (Math.abs(dx) > Math.abs(dy) && pictures.length > 1) move(dx < 0 ? 1 : -1)
        }}>
        <button type="button" className="image-viewer-picture" aria-pressed={zoom} aria-label={zoom ? t('點一下縮回整張') : t('點一下放大')}
          onClick={() => { if (!suppressClick.current) setZoom((value) => !value); suppressClick.current = false }}>
          <img src={picture.src} alt={picture.name} draggable={false} />
        </button>
      </div>
      {pictures.length > 1 ? <nav className="image-viewer-nav" aria-label={t('圖片')}>
        <button type="button" aria-label={t('上一張')} onClick={() => move(-1)}>←</button>
        <button type="button" aria-label={t('下一張')} onClick={() => move(1)}>→</button>
      </nav> : null}
      <footer className="image-viewer-bar">
        <span className="image-viewer-hint">{t('點圖放大 · 點背景或下滑關閉')}</span>
        <a href={picture.src} target="_blank" rel="noopener noreferrer">{t('原圖')}</a>
        <a href={picture.src} download={picture.name}>{t('下載')}</a>
      </footer>
    </div>, document.body,
  )
}
