/**
 * 左上角的「重建 N/5」chip（使用者 2026-09-14）：累積夠多重建申請時，AGM 的排程不等整點就會
 * 動手，所以這個數字要看得到。點開列出是誰申請的、哪個 commit、要做什麼。
 *
 * 一筆都沒有就整顆不出現——左上角那一列很窄，常態是 0，畫一顆永遠寫 0 的 chip 只是噪音。
 */
import { useEffect, useRef, useState } from 'react'
import { fetchRebuildRequests, REBUILD_THRESHOLD } from '../api/rebuildRequests'
import type { RebuildRequest } from '../lib/rebuildCount'
import './rebuildBadge.css'

/** 申請是人在動的，不必跟得比這更緊。 */
const POLL_MS = 30_000

export function RebuildBadge() {
  const [rows, setRows] = useState<RebuildRequest[] | null>(null)
  const [open, setOpen] = useState(false)
  const boxRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    let live = true
    const refresh = async () => {
      const next = await fetchRebuildRequests()
      if (live) setRows(next)
    }
    void refresh()
    const t = setInterval(() => void refresh(), POLL_MS)
    return () => {
      live = false
      clearInterval(t)
    }
  }, [])

  useEffect(() => {
    if (!open) return
    const away = (e: MouseEvent) => {
      if (!boxRef.current?.contains(e.target as Node)) setOpen(false)
    }
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && setOpen(false)
    document.addEventListener('mousedown', away)
    document.addEventListener('keydown', esc)
    return () => {
      document.removeEventListener('mousedown', away)
      document.removeEventListener('keydown', esc)
    }
  }, [open])

  if (!rows || rows.length === 0) return null
  const hot = rows.length >= REBUILD_THRESHOLD

  return (
    <div className="rebuild-badge-box" ref={boxRef}>
      <button
        type="button"
        className={`rebuild-badge${hot ? ' hot' : ''}`}
        aria-label={`還在等的重建申請 ${rows.length} 筆，門檻 ${REBUILD_THRESHOLD} 筆`}
        onClick={() => setOpen((v) => !v)}
        title={
          hot
            ? `重建申請已達 ${REBUILD_THRESHOLD} 筆：AGM 不等整點，下一輪檢查就會安排重建。`
            : `還在等的重建申請 ${rows.length} 筆；滿 ${REBUILD_THRESHOLD} 筆就不等整點。`
        }
      >
        <span className="rebuild-k" aria-hidden="true">⟳</span>
        <span className="rebuild-v">
          {rows.length}/{REBUILD_THRESHOLD}
        </span>
      </button>
      {open ? (
        <div className="rebuild-pop" role="dialog" aria-label="重建申請">
          <div className="rebuild-pop-head">
            還在等的重建申請 {rows.length} / {REBUILD_THRESHOLD}
          </div>
          <ul className="rebuild-list">
            {rows.map((r) => (
              <li key={r.id}>
                <span className="rebuild-who">{r.requester || '（不明）'}</span>
                <span className="rebuild-commit mono">{r.target_commit.slice(0, 7) || '—'}</span>
                <span className={`rebuild-status${r.status === 'approved' ? ' ok' : ''}`}>
                  {r.status === 'approved' ? '已核准' : '待裁示'}
                </span>
                <span className="rebuild-scope" title={r.scope}>
                  {r.scope}
                </span>
              </li>
            ))}
          </ul>
          <div className="rebuild-pop-foot">滿 {REBUILD_THRESHOLD} 筆時，AGM 的排程不等整點就會安排重建。</div>
        </div>
      ) : null}
    </div>
  )
}
