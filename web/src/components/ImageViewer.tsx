import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { useT } from '../lib/i18n'
import './imageViewer.css'

export type Picture = { src: string; name: string }

const MIN_SCALE = 1
const MAX_SCALE = 5
const TAP_ZOOM = 2

type Pt = { x: number; y: number }
type SingleTouch = { x: number; y: number; zoomed: boolean; ox: number; oy: number; moved: boolean }
type PinchTouch = { startDist: number; startMid: Pt; startScale: number; startOffset: Pt }

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v))

function touchMetrics(t0: { clientX: number; clientY: number }, t1: { clientX: number; clientY: number }) {
  const dx = t1.clientX - t0.clientX
  const dy = t1.clientY - t0.clientY
  return {
    dist: Math.max(1, Math.hypot(dx, dy)),
    mid: { x: (t0.clientX + t1.clientX) / 2, y: (t0.clientY + t1.clientY) / 2 },
  }
}

export function ImageViewer({ pictures, index: initialIndex, onClose }: { pictures: Picture[]; index: number; onClose: () => void }) {
  const t = useT()
  const [index, setIndex] = useState(initialIndex)
  const [scale, setScale] = useState(MIN_SCALE)
  const [offset, setOffset] = useState<Pt>({ x: 0, y: 0 })
  const root = useRef<HTMLDivElement>(null)
  const viewport = useRef<HTMLDivElement>(null)
  const imgRef = useRef<HTMLImageElement>(null)
  const scaleRef = useRef(MIN_SCALE)
  const offsetRef = useRef<Pt>({ x: 0, y: 0 })
  const touch = useRef<SingleTouch | null>(null)
  const pinch = useRef<PinchTouch | null>(null)
  const pinched = useRef(false)
  const suppressClick = useRef(false)
  const close = useRef(onClose)
  useEffect(() => { close.current = onClose }, [onClose])

  const picture = pictures[index]
  const zoomed = scale > 1.01

  const clampOffset = (x: number, y: number, s: number): Pt => {
    if (s <= 1.01) return { x: 0, y: 0 }
    const stage = viewport.current
    const img = imgRef.current
    if (!stage || !img || !stage.clientWidth || !img.clientWidth) return { x, y }
    const maxX = Math.max(0, (img.clientWidth * s - stage.clientWidth) / 2)
    const maxY = Math.max(0, (img.clientHeight * s - stage.clientHeight) / 2)
    return { x: clamp(x, -maxX, maxX), y: clamp(y, -maxY, maxY) }
  }

  const applyView = (nextScale: number, nextOffset: Pt) => {
    const s = clamp(nextScale, MIN_SCALE, MAX_SCALE)
    const o = clampOffset(nextOffset.x, nextOffset.y, s)
    scaleRef.current = s
    offsetRef.current = o
    setScale(s)
    setOffset(o)
  }

  const resetView = () => applyView(MIN_SCALE, { x: 0, y: 0 })

  const move = (delta: number) => {
    setIndex((i) => (i + delta + pictures.length) % pictures.length)
    resetView()
  }

  useDialogFocus(true, root)

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
        scaleRef.current = MIN_SCALE
        offsetRef.current = { x: 0, y: 0 }
        setScale(MIN_SCALE)
        setOffset({ x: 0, y: 0 })
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

  const startPinch = (t0: { clientX: number; clientY: number }, t1: { clientX: number; clientY: number }) => {
    const { dist, mid } = touchMetrics(t0, t1)
    pinch.current = { startDist: dist, startMid: mid, startScale: scaleRef.current, startOffset: offsetRef.current }
    touch.current = null
    pinched.current = true
    suppressClick.current = true
  }

  return createPortal(
    <div ref={root} className="image-viewer" role="dialog" aria-modal="true" aria-label={t('預覽 {name}', { name: picture.name })} tabIndex={-1}
      onClick={(event) => { event.stopPropagation(); if (event.target === event.currentTarget) onClose() }}>
      <header className="image-viewer-bar">
        <span className="image-viewer-name" title={picture.name}>{picture.name}</span>
        {zoomed ? (
          <button type="button" className="image-viewer-scale" onClick={resetView} aria-label={t('點一下縮回整張')}>
            {scale.toFixed(1)}×
          </button>
        ) : null}
        <span aria-live="polite">{index + 1} / {pictures.length}</span>
        <button type="button" onClick={onClose} aria-label={t('關閉')}>×</button>
      </header>
      <div ref={viewport} className={`image-viewer-stage${zoomed ? ' zoomed' : ''}`}
        onClick={(event) => { if (event.target === event.currentTarget && !suppressClick.current) onClose(); suppressClick.current = false }}
        onWheel={(event) => {
          if (!event.ctrlKey && !event.metaKey) return
          event.preventDefault()
          const factor = Math.exp(-event.deltaY * 0.01)
          const next = clamp(scaleRef.current * factor, MIN_SCALE, MAX_SCALE)
          if (next <= 1.02) { resetView(); return }
          const rect = viewport.current?.getBoundingClientRect()
          const cx = rect ? event.clientX - (rect.left + rect.width / 2) : 0
          const cy = rect ? event.clientY - (rect.top + rect.height / 2) : 0
          const ratio = next / scaleRef.current
          applyView(next, {
            x: cx - (cx - offsetRef.current.x) * ratio,
            y: cy - (cy - offsetRef.current.y) * ratio,
          })
        }}
        onTouchStart={(event) => {
          if (event.touches.length >= 2) {
            startPinch(event.touches[0], event.touches[1])
            return
          }
          if (event.touches.length === 1) {
            suppressClick.current = false
            touch.current = {
              x: event.touches[0].clientX,
              y: event.touches[0].clientY,
              zoomed: scaleRef.current > 1.01,
              ox: offsetRef.current.x,
              oy: offsetRef.current.y,
              moved: false,
            }
          }
        }}
        onTouchMove={(event) => {
          if (event.touches.length >= 2) {
            if (!pinch.current) startPinch(event.touches[0], event.touches[1])
            const p = pinch.current!
            const { dist, mid } = touchMetrics(event.touches[0], event.touches[1])
            const nextScale = clamp(p.startScale * (dist / p.startDist), MIN_SCALE, MAX_SCALE)
            const rect = viewport.current?.getBoundingClientRect()
            const cx = rect ? p.startMid.x - (rect.left + rect.width / 2) : 0
            const cy = rect ? p.startMid.y - (rect.top + rect.height / 2) : 0
            const ratio = nextScale / p.startScale
            applyView(nextScale, {
              x: cx - (cx - p.startOffset.x) * ratio + (mid.x - p.startMid.x),
              y: cy - (cy - p.startOffset.y) * ratio + (mid.y - p.startMid.y),
            })
            return
          }
          const start = touch.current
          if (!start || !event.touches.length) return
          const dx = event.touches[0].clientX - start.x
          const dy = event.touches[0].clientY - start.y
          if (start.zoomed) {
            if (Math.hypot(dx, dy) > 4) {
              start.moved = true
              suppressClick.current = true
            }
            applyView(scaleRef.current, { x: start.ox + dx, y: start.oy + dy })
          }
        }}
        onTouchEnd={(event) => {
          if (event.touches.length >= 2) {
            startPinch(event.touches[0], event.touches[1])
            return
          }
          if (event.touches.length === 1 && pinch.current) {
            pinch.current = null
            if (scaleRef.current > 1.01) {
              touch.current = {
                x: event.touches[0].clientX,
                y: event.touches[0].clientY,
                zoomed: true,
                ox: offsetRef.current.x,
                oy: offsetRef.current.y,
                moved: true,
              }
            } else {
              touch.current = null
            }
            return
          }
          const start = touch.current
          touch.current = null
          pinch.current = null
          if (pinched.current) {
            pinched.current = false
            suppressClick.current = true
            if (scaleRef.current < 1.05) resetView()
            return
          }
          if (!start || !event.changedTouches.length) return
          const dx = event.changedTouches[0].clientX - start.x
          const dy = event.changedTouches[0].clientY - start.y
          if (start.zoomed) {
            if (start.moved || Math.max(Math.abs(dx), Math.abs(dy)) > 10) suppressClick.current = true
            return
          }
          if (Math.max(Math.abs(dx), Math.abs(dy)) < 50) return
          suppressClick.current = true
          if (dy > 70 && dy > Math.abs(dx)) onClose()
          else if (Math.abs(dx) > Math.abs(dy) && pictures.length > 1) move(dx < 0 ? 1 : -1)
        }}
        onTouchCancel={() => {
          touch.current = null
          pinch.current = null
          pinched.current = false
        }}>
        <button type="button" className="image-viewer-picture" aria-pressed={zoomed} aria-label={zoomed ? t('點一下縮回整張') : t('點一下放大')}
          style={zoomed ? { transform: `translate3d(${offset.x}px, ${offset.y}px, 0) scale(${scale})` } : undefined}
          onClick={() => {
            if (suppressClick.current) { suppressClick.current = false; return }
            if (scaleRef.current > 1.01) resetView()
            else applyView(TAP_ZOOM, { x: 0, y: 0 })
          }}>
          <img ref={imgRef} src={picture.src} alt={picture.name} draggable={false} />
        </button>
      </div>
      {pictures.length > 1 ? <nav className="image-viewer-nav" aria-label={t('圖片')}>
        <button type="button" aria-label={t('上一張')} onClick={() => move(-1)}>←</button>
        <button type="button" aria-label={t('下一張')} onClick={() => move(1)}>→</button>
      </nav> : null}
      <footer className="image-viewer-bar">
        <span className="image-viewer-hint">{t('點圖或雙指放大 · 點背景或下滑關閉')}</span>
        <a href={picture.src} target="_blank" rel="noopener noreferrer">{t('原圖')}</a>
        <a href={picture.src} download={picture.name}>{t('下載')}</a>
      </footer>
    </div>, document.body,
  )
}
