import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { fetchMemPane } from '../api'
import type { MemProcess, TerminalSnapshot } from '../api/types'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { useT } from '../lib/i18n'

const REFRESH_MS = 2000
const LINES = 200

export function MemPaneModal({ host, p, onClose }: { host: string; p: MemProcess; onClose: () => void }) {
  const t = useT()
  const paneId = p.pane_id ?? ''
  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const rootRef = useRef<HTMLDivElement>(null)
  const closeRef = useRef<HTMLButtonElement>(null)
  const termRef = useRef<HTMLPreElement>(null)
  useDialogFocus(true, rootRef, { initialFocus: () => closeRef.current })

  useEffect(() => {
    let alive = true
    let busy = false
    let timer: ReturnType<typeof setTimeout> | null = null
    const next = () => {
      if (alive) timer = setTimeout(() => void tick(), REFRESH_MS)
    }
    const tick = async () => {
      if (document.visibilityState === 'hidden') {
        next()
        return
      }
      busy = true
      try {
        const s = await fetchMemPane(host, paneId, p.socket_path, LINES)
        if (alive) {
          setSnap(s)
          setErr(null)
        }
      } catch (e: unknown) {
        if (alive) setErr(e instanceof Error ? e.message : t('讀不到'))
      }
      busy = false
      next()
    }
    const onVisible = () => {
      if (document.visibilityState !== 'visible' || busy) return
      if (timer) clearTimeout(timer)
      timer = null
      void tick()
    }
    document.addEventListener('visibilitychange', onVisible)
    void tick()
    return () => {
      alive = false
      document.removeEventListener('visibilitychange', onVisible)
      if (timer) clearTimeout(timer)
    }
  }, [host, paneId, p.socket_path, t])

  const scrolled = useRef(false)
  useEffect(() => {
    if (snap && !scrolled.current && termRef.current) {
      termRef.current.scrollTop = termRef.current.scrollHeight
      scrolled.current = true
    }
  }, [snap])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      e.preventDefault()
      e.stopPropagation()
      onClose()
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [onClose])

  return createPortal(
    <div className="modal-backdrop mem-pane-backdrop" role="presentation" onMouseDown={onClose}>
      <div
        ref={rootRef}
        className="modal blocked-modal mem-pane-modal"
        role="dialog"
        aria-modal="true"
        aria-label={t('pane {paneId} 的畫面', { paneId })}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <strong className="blocked-title">pane {paneId}</strong>
          <span className="modal-sub" title={p.argv}>
            {p.argv}
            {snap?.columns ? ` · ${snap.columns}×${snap.rows ?? '?'}` : ''}
            {` · ${t('只讀，每 2 秒更新')}`}
          </span>
          <button ref={closeRef} type="button" className="icon-btn" aria-label={t('關閉')} title={t('關閉（Esc）')} onClick={onClose}>
            ✕
          </button>
        </div>
        <pre className="term blocked-modal-term mem-pane-term" ref={termRef} tabIndex={0}>
          {err ? t('讀取畫面失敗：{err}', { err }) : (snap?.text.replace(/\s+$/, '') || (snap ? t('（畫面是空的）') : t('讀取中…')))}
        </pre>
      </div>
    </div>,
    document.body,
  )
}
