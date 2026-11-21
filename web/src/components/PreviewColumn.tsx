import { useCallback, useEffect, useRef, useState } from 'react'
import type { KeyboardEvent, PointerEvent as ReactPointerEvent } from 'react'
import { DRAWER_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { IN_MOBILE_PREVIEW } from '../store/mobilePreview'
import {
  PREVIEW_COL_MIN,
  PREVIEW_RAIL_W,
  clampPreviewWidth,
  defaultPreviewWidth,
  setPreviewColOpen,
  setPreviewColWidth,
  usePreviewColOpen,
  usePreviewColWidth,
} from '../store/previewLayout'
import { isTopLevelBot } from '../store/routeSync'
import { useStore } from '../store/store'
import { PreviewPanel } from './PreviewPanel'
import { useT } from '../lib/i18n'
import './previewColumn.css'

const KEY_STEP = 24

export function PreviewColumn() {
  const t = useT()
  const drawer = useMediaQuery(DRAWER_QUERY)
  const bot = useStore((s) => (s.selectedProjectId ? null : (s.bots.find((b) => b.id === s.selectedBotId) ?? null)))
  const open = usePreviewColOpen(bot?.id ?? null)
  const running = useStore((s) => (bot ? s.previews[bot.id]?.status === 'running' : false))
  const stored = usePreviewColWidth()
  const [viewport, setViewport] = useState(() => window.innerWidth)
  const [drag, setDrag] = useState<number | null>(null)
  const start = useRef<{ x: number; w: number } | null>(null)

  useEffect(() => {
    const on = () => setViewport(window.innerWidth)
    window.addEventListener('resize', on)
    return () => window.removeEventListener('resize', on)
  }, [])

  const base = clampPreviewWidth(stored ?? defaultPreviewWidth(viewport), viewport)
  const width = drag ?? base

  const onDown = useCallback(
    (e: ReactPointerEvent<HTMLDivElement>) => {
      e.preventDefault()
      e.currentTarget.setPointerCapture(e.pointerId)
      start.current = { x: e.clientX, w: base }
      setDrag(base)
    },
    [base],
  )
  const onMove = useCallback(
    (e: ReactPointerEvent<HTMLDivElement>) => {
      const s = start.current
      if (!s) return
      setDrag(clampPreviewWidth(s.w + (s.x - e.clientX), window.innerWidth))
    },
    [],
  )
  const onUp = useCallback(() => {
    if (!start.current) return
    start.current = null
    setDrag((w) => {
      if (w !== null) setPreviewColWidth(w)
      return null
    })
  }, [])
  const maxWidth = Math.max(PREVIEW_COL_MIN, Math.floor(viewport * 0.5))
  const onKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Home' || e.key === 'End') {
      e.preventDefault()
      setPreviewColWidth(e.key === 'Home' ? PREVIEW_COL_MIN : maxWidth)
      return
    }
    const d = e.key === 'ArrowLeft' ? KEY_STEP : e.key === 'ArrowRight' ? -KEY_STEP : 0
    if (!d) return
    e.preventDefault()
    setPreviewColWidth(clampPreviewWidth(base + d, viewport))
  }

  if (drawer || IN_MOBILE_PREVIEW || !bot || !isTopLevelBot(bot)) return null

  const merged = running
  const headStart = (
    <>
      <span className="preview-col-title">{t('預覽')}</span>
      <span className="preview-col-bot" title={bot.name}>
        {bot.name}
      </span>
    </>
  )
  const collapse = (
    <button
      type="button"
      className="icon-btn preview-col-toggle"
      title={t('收合預覽')}
      aria-label={t('收合預覽')}
      aria-expanded
      onClick={() => setPreviewColOpen(bot.id, false)}
    >
      ▸
    </button>
  )

  if (!open) {
    return (
      <aside className="preview-col rail" style={{ width: PREVIEW_RAIL_W }} aria-label={t('預覽')}>
        <button
          type="button"
          className="icon-btn preview-col-toggle"
          title={t('展開預覽')}
          aria-label={t('展開預覽')}
          aria-expanded={false}
          onClick={() => {
            setPreviewColOpen(bot.id, true)
            window.dispatchEvent(new CustomEvent('am:shelf-collapse'))
          }}
        >
          ◧
        </button>
        <span className="preview-col-rail-label">{t('預覽')}</span>
      </aside>
    )
  }

  return (
    <aside className={`preview-col${drag !== null ? ' dragging' : ''}`} style={{ width }} aria-label={t('預覽')}>
      <div
        className="preview-col-grip"
        role="separator"
        aria-orientation="vertical"
        aria-label={t('調整預覽寬度')}
        aria-valuenow={width}
        aria-valuemin={PREVIEW_COL_MIN}
        aria-valuemax={maxWidth}
        aria-valuetext={t('{width} 像素', { width })}
        tabIndex={0}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        onKeyDown={onKey}
      />
      {merged ? null : (
        <div className="preview-col-head">
          {headStart}
          {collapse}
        </div>
      )}
      <div className="preview-col-body">
        <PreviewPanel key={bot.id} botId={bot.id} {...(merged ? { headStart, headEnd: collapse } : {})} />
      </div>
    </aside>
  )
}
