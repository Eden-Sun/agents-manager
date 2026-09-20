/**
 * 桌機 `.app` grid 最右邊那一欄（檔案暫存的右邊）：選著頂層 bot 時放它的預覽（issue #253 v3）。
 * 掛在 `main` 外（同 ImageShelf）：換 bot 不 unmount，同一顆的 iframe 不重載。
 * ≤1024px（抽屜版面／手機）沒有這欄，預覽回到 ChatPanel 的分頁。
 */
import { useCallback, useEffect, useRef, useState } from 'react'
import type { KeyboardEvent, PointerEvent as ReactPointerEvent } from 'react'
import { DRAWER_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { IN_MOBILE_PREVIEW } from '../store/mobilePreview'
import {
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
import './previewColumn.css'

const KEY_STEP = 24

export function PreviewColumn() {
  const drawer = useMediaQuery(DRAWER_QUERY)
  const bot = useStore((s) => (s.selectedProjectId ? null : (s.bots.find((b) => b.id === s.selectedBotId) ?? null)))
  const open = usePreviewColOpen()
  const stored = usePreviewColWidth()
  const [viewport, setViewport] = useState(() => window.innerWidth)
  // 拖曳中的即時寬度；放開才寫進 localStorage。
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
      // 手柄在欄的左緣：往左拖＝變寬。
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
  const onKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const d = e.key === 'ArrowLeft' ? KEY_STEP : e.key === 'ArrowRight' ? -KEY_STEP : 0
    if (!d) return
    e.preventDefault()
    setPreviewColWidth(clampPreviewWidth(base + d, viewport))
  }

  if (drawer || IN_MOBILE_PREVIEW || !bot || !isTopLevelBot(bot)) return null

  if (!open) {
    return (
      <aside className="preview-col rail" style={{ width: PREVIEW_RAIL_W }} aria-label="預覽">
        <button
          type="button"
          className="icon-btn preview-col-toggle"
          title="展開預覽"
          aria-label="展開預覽"
          aria-expanded={false}
          onClick={() => setPreviewColOpen(true)}
        >
          ◧
        </button>
        <span className="preview-col-rail-label">預覽</span>
      </aside>
    )
  }

  return (
    <aside className={`preview-col${drag !== null ? ' dragging' : ''}`} style={{ width }} aria-label="預覽">
      <div
        className="preview-col-grip"
        role="separator"
        aria-orientation="vertical"
        aria-label="調整預覽寬度"
        aria-valuenow={width}
        tabIndex={0}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        onKeyDown={onKey}
      />
      <div className="preview-col-head">
        <span className="preview-col-title">預覽</span>
        <span className="preview-col-bot" title={bot.name}>
          {bot.name}
        </span>
        <button
          type="button"
          className="icon-btn preview-col-toggle"
          title="收合預覽"
          aria-label="收合預覽"
          aria-expanded
          onClick={() => setPreviewColOpen(false)}
        >
          ▸
        </button>
      </div>
      <div className="preview-col-body">
        <PreviewPanel key={bot.id} botId={bot.id} />
      </div>
    </aside>
  )
}
