/**
 * 左上角的「重建 N/5」chip（使用者 2026-09-14）：累積夠多重建申請、或最早一筆等超過 30 分鐘（09-15）時，AGM 的排程不等整點就會
 * 動手，所以這個數字要看得到。點開列出是誰申請的、哪個 commit、要做什麼。
 *
 * 一筆都沒有就整顆不出現——左上角那一列很窄，常態是 0，畫一顆永遠寫 0 的 chip 只是噪音。
 */
import { useEffect, useRef, useState } from 'react'
import { fetchRebuildRequests, REBUILD_MAX_WAIT_MIN, REBUILD_THRESHOLD, supervisorBotId } from '../api/rebuildRequests'
import * as api from '../api'
import { useStore } from '../store/store'
import { oldestWaitMinutes, type RebuildRequest } from '../lib/rebuildCount'
import { rebuildAsker, rebuildAskNotice } from '../lib/rebuildAsk'
import './rebuildBadge.css'

/** 申請是人在動的，不必跟得比這更緊。 */
const POLL_MS = 30_000

/** 沒送出就沿用同一個 crid，重按不會變成兩則「現在重建」。 */
const askAgm = rebuildAsker(api.newClientRequestId)

export function RebuildBadge() {
  const [rows, setRows] = useState<RebuildRequest[] | null>(null)
  /** 上一次沒問到（daemon 多半正在重啟）：數字留著，但要標成不保證是現況（issue #531）。 */
  const [offline, setOffline] = useState(false)
  const [open, setOpen] = useState(false)
  const [asking, setAsking] = useState(false)
  const notify = useStore((s) => s.notify)
  const boxRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    let live = true
    const refresh = async () => {
      const snap = await fetchRebuildRequests()
      if (!live) return
      setOffline(snap.offline)
      // 連不上就留著上一次的數字（標成過期）；daemon 自己說沒有才收掉。
      if (!snap.offline) setRows(snap.rows)
    }
    // `fetchRebuildRequests` 已經不會拋了，這裡再接一次：輪詢的 promise 沒人接就是 unhandled rejection。
    const tick = () => void refresh().catch(() => {})
    tick()
    const t = setInterval(tick, POLL_MS)
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
  const waited = oldestWaitMinutes(rows)
  // 集滿門檻、或最早那筆等太久：下一輪檢查就會安排重建（與 daemon-update-kick.sh 同一套）。
  const full = rows.length >= REBUILD_THRESHOLD
  const hot = full || waited >= REBUILD_MAX_WAIT_MIN

  return (
    <div className="rebuild-badge-box" ref={boxRef}>
      <button
        type="button"
        className={`rebuild-badge${hot && !offline ? ' hot' : ''}${offline ? ' offline' : ''}`}
        aria-label={`還在等的重建申請 ${rows.length} 筆，門檻 ${REBUILD_THRESHOLD} 筆${offline ? '（連不上 daemon，數字是斷線前的）' : ''}`}
        onClick={() => setOpen((v) => !v)}
        title={
          offline
            ? `連不上 daemon（可能正在重啟），這個數字是斷線前的 ${rows.length} 筆，不一定是現況。`
            : full
            ? `重建申請已達 ${REBUILD_THRESHOLD} 筆：AGM 不等整點，下一輪檢查就會安排重建。`
            : hot
              ? `最早一筆重建申請已等 ${waited} 分鐘：AGM 不等整點，下一輪檢查就會安排重建。`
              : `還在等的重建申請 ${rows.length} 筆（最早一筆等了 ${waited} 分鐘）；滿 ${REBUILD_THRESHOLD} 筆或等滿 ${REBUILD_MAX_WAIT_MIN} 分鐘就不等整點。`
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
            {offline ? <span className="rebuild-offline-note">連不上 daemon，以下是斷線前的</span> : null}
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
          {/* 一鍵催 AGM（2026-09-16 使用者）：送一則使用者訊息給總管，不自己動 build。 */}
          <button
            type="button"
            className="mini-btn rebuild-ask"
            disabled={asking}
            onClick={() => {
              setAsking(true)
              void (async () => {
                const agm = await supervisorBotId()
                if (!agm) {
                  notify('error', '找不到 AGM 這顆 bot，沒送出')
                  setAsking(false)
                  return
                }
                const outcome = await askAgm((crid) =>
                  api.sendPrompt(
                    agm,
                    `使用者要求：現在開始重建 release 並重啟 daemon（還在等的重建申請 ${rows.length} 筆，最早一筆等了 ${waited} 分鐘）。請照既有流程排程，完成後回報。`,
                    crid,
                  ),
                )
                const n = rebuildAskNotice(outcome)
                notify(n.level, n.text)
                if (n.close) setOpen(false)
                setAsking(false)
              })()
            }}
          >
            {asking ? '送出中…' : '請 AGM 現在重建'}
          </button>
          <div className="rebuild-pop-foot">
            滿 {REBUILD_THRESHOLD} 筆、或最早一筆等滿 {REBUILD_MAX_WAIT_MIN} 分鐘（現在 {waited} 分鐘），AGM 的排程就不等整點安排重建。
          </div>
        </div>
      ) : null}
    </div>
  )
}
