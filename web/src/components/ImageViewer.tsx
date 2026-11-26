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
  const touch = useRef<{ x: number; y: number; zoomed: boolean; sx: number; sy: number } | null>(null)
  const suppressClick = useRef(false)
  const close = useRef(onClose)
  useEffect(() => { close.current = onClose }, [onClose])
  const picture = pictures[index]
  const resetScroll = () => {
    if (viewport.current) { viewport.current.scrollTop = 0; viewport.current.scrollLeft = 0 }
  }
  const move = (delta: number) => {
    setIndex((i) => (i + delta + pictures.length) % pictures.length)
    setZoom(false)
    resetScroll()
  }
  useDialogFocus(true, root)
  useEffect(() => {
    const el = viewport.current
    if (!el) return
    if (zoom) {
      el.scrollLeft = Math.max(0, Math.round((el.scrollWidth - el.clientWidth) / 2))
      el.scrollTop = Math.max(0, Math.round((el.scrollHeight - el.clientHeight) / 2))
    } else {
      el.scrollLeft = 0
      el.scrollTop = 0
    }
  }, [zoom, index])
  useEffect(() => {
    const body = document.body
    const html = document.documentElement
    const savedBody = { position: body.style.position, top: body.style.top, left: body.style.left, right: body.style.right, width: body.style.width, overflow: body.style.overflow }
    const savedHtml = { overflow: html.style.overflow, overscrollBehavior: html.style.overscrollBehavior }
    const x = window.scrollX, y = window.scrollY
    Object.assign(body.style, { position: 'fixed', top: `${-y}px`, left: '0', right: '0', width: '100%', overflow: 'hidden' })
    Object.assign(html.style, { overflow: 'hidden', overscrollBehavior: 'none' })
    const key = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing) return
      if (event.key === 'Escape') { event.preventDefault(); close.current() }
      if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
        event.preventDefault()
        setIndex((i) => (i + (event.key === 'ArrowLeft' ? -1 : 1) + pictures.length) % pictures.length)
        setZoom(false)
      }
    }
    const el = root.current
    const blockPinch = (event: TouchEvent) => {
      if (event.touches.length > 1) event.preventDefault()
    }
    const blockGesture = (event: Event) => {
      event.preventDefault()
    }
    window.addEventListener('keydown', key)
    el?.addEventListener('touchmove', blockPinch, { passive: false })
    el?.addEventListener('gesturestart', blockGesture, { passive: false })
    el?.addEventListener('gesturechange', blockGesture, { passive: false })
    return () => {
      window.removeEventListener('keydown', key)
      el?.removeEventListener('touchmove', blockPinch)
      el?.removeEventListener('gesturestart', blockGesture)
      el?.removeEventListener('gesturechange', blockGesture)
      Object.assign(body.style, savedBody)
      Object.assign(html.style, savedHtml)
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
        onClick={(event) => { if (event.target === event.currentTarget && !suppressClick.current) onClose(); suppressClick.current = false }}
        onTouchStart={(event) => {
          suppressClick.current = false
          if (event.touches.length === 1) {
            const el = viewport.current
            touch.current = { x: event.touches[0].clientX, y: event.touches[0].clientY, zoomed: zoom, sx: el?.scrollLeft ?? 0, sy: el?.scrollTop ?? 0 }
          } else {
            touch.current = null
            suppressClick.current = true
          }
        }}
        onTouchMove={(event) => {
          if (event.touches.length > 1) {
            touch.current = null
            suppressClick.current = true
            return
          }
          const start = touch.current
          if (!start || !event.touches.length) return
          if (start.zoomed) {
            const dx = Math.abs(event.touches[0].clientX - start.x)
            const dy = Math.abs(event.touches[0].clientY - start.y)
            const el = viewport.current
            if (Math.max(dx, dy) > 10 || (el && (el.scrollLeft !== start.sx || el.scrollTop !== start.sy))) {
              suppressClick.current = true
            }
          }
        }}
        onTouchEnd={(event) => {
          const start = touch.current
          touch.current = null
          if (!start || !event.changedTouches.length) return
          const dx = event.changedTouches[0].clientX - start.x, dy = event.changedTouches[0].clientY - start.y
          if (start.zoomed) {
            const el = viewport.current
            if (Math.max(Math.abs(dx), Math.abs(dy)) > 10 || (el && (el.scrollLeft !== start.sx || el.scrollTop !== start.sy))) {
              suppressClick.current = true
            }
            return
          }
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
