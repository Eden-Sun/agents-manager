import { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { fetchMemPane } from '../api'
import type { MemProcess, TerminalSnapshot } from '../api/types'
import { useDialogFocus } from '../hooks/useDialogFocus'

/**
 * RAM 清單裡點 pane 後看它現在的畫面（唯讀）。用寬 modal 而不是列內攤開：
 * 560px 塞 185 欄會每行折斷，認不出是哪個 claude。
 */
const REFRESH_MS = 2000
const LINES = 200

export function MemPaneModal({ host, p, onClose }: { host: string; p: MemProcess; onClose: () => void }) {
  const paneId = p.pane_id ?? ''
  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const rootRef = useRef<HTMLDivElement>(null)
  const closeRef = useRef<HTMLButtonElement>(null)
  const termRef = useRef<HTMLPreElement>(null)
  useDialogFocus(true, rootRef, { initialFocus: () => closeRef.current })

  useEffect(() => {
    let alive = true
    const tick = () =>
      fetchMemPane(host, paneId, p.socket_path, LINES)
        .then((s) => {
          if (!alive) return
          setSnap(s)
          setErr(null)
        })
        .catch((e: unknown) => alive && setErr(e instanceof Error ? e.message : '讀不到'))
    void tick()
    const t = setInterval(() => void tick(), REFRESH_MS)
    return () => {
      alive = false
      clearInterval(t)
    }
  }, [host, paneId, p.socket_path])

  // 第一次載入後捲到底：終端的「現在」在最下面。
  const scrolled = useRef(false)
  useEffect(() => {
    if (snap && !scrolled.current && termRef.current) {
      termRef.current.scrollTop = termRef.current.scrollHeight
      scrolled.current = true
    }
  }, [snap])

  // Esc 只關這個 modal，不連清單一起關；捕獲階段吃掉，清單的 Esc 監聽就看不到。
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
        aria-label={`pane ${paneId} 的畫面`}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <strong className="blocked-title">pane {paneId}</strong>
          <span className="modal-sub" title={p.argv}>
            {p.argv}
            {snap?.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
            ・只讀，每 2 秒更新
          </span>
          <button ref={closeRef} type="button" className="icon-btn" aria-label="關閉" title="關閉（Esc）" onClick={onClose}>
            ✕
          </button>
        </div>
        <pre className="term blocked-modal-term mem-pane-term" ref={termRef} tabIndex={0}>
          {err ? `讀取畫面失敗：${err}` : (snap?.text.replace(/\s+$/, '') || (snap ? '（畫面是空的）' : '讀取中…'))}
        </pre>
      </div>
    </div>,
    document.body,
  )
}
