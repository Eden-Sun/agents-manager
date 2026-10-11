/**
 * bot 上的「Jev」徽章（issue #1328）：近 24 小時有超過門檻的第二意見時才出現，點開看場景、機率、時間與遮罩後的命中行。
 * 只是提示：寫明「不會自動動作」。關掉的那一則記在這台瀏覽器，不再顯示。
 */
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { JUDGE_SCENES, probabilityText } from '../lib/judgeStats'
import { judgeBadgeFor, popPosition } from '../lib/judgeBadge'
import { useJudgeLedger } from '../store/judgeLedger'
import { useStore } from '../store/store'
import './judgeBadge.css'

export function JudgeBadge({ botId }: { botId: string }) {
  const visible = useJudgeLedger((s) => s.visible)
  const rows = useJudgeLedger((s) => s.rows)
  const dismissed = useJudgeLedger((s) => s.dismissed)
  const snapshotAt = useJudgeLedger((s) => s.lastFetchAt)
  const dismiss = useJudgeLedger((s) => s.dismiss)
  /** 開著浮層的那一則 id：那一則不在了（關掉、換了、過期），浮層自然不顯示，不用 effect 收。 */
  const [openId, setOpenId] = useState<string | null>(null)
  const wrapRef = useRef<HTMLSpanElement>(null)
  const btnRef = useRef<HTMLButtonElement>(null)
  const popRef = useRef<HTMLDivElement>(null)
  /** 「關掉這則」之後要還焦點：`fallback` 是事先記下的最近可聚焦祖先（列），徽章不在了才用它。 */
  const restoreFocus = useRef<{ fallback: HTMLElement | null } | null>(null)
  /** 浮層的視窗座標（fixed）：打開時量徽章鈕算出來，不靠側欄的捲動容器（#1381）。 */
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  // 「近 24 小時」以快照的時間為準（每次重抓都會更新），渲染只依賴 store，不讀牆鐘。
  const row = visible ? judgeBadgeFor(rows, botId, snapshotAt, dismissed) : null
  const open = row !== null && openId === row.id
  // 打開：焦點進浮層；點浮層外面或 Esc（不是組字中）：關掉，Esc 時焦點回徽章鈕（同 HeadMoreMenu）。
  useEffect(() => {
    if (!open) return
    popRef.current?.focus({ preventScroll: true })
    const onDoc = (e: MouseEvent) => {
      if (wrapRef.current && !wrapRef.current.contains(e.target as Node)) setOpenId(null)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !e.isComposing) {
        setOpenId(null)
        btnRef.current?.focus()
      }
    }
    // fixed 的浮層不會跟著列捲動：打開期間捲動或改視窗大小就關掉，不留在錯的位置（#1381）。
    const onMove = () => setOpenId(null)
    document.addEventListener('mousedown', onDoc)
    document.addEventListener('keydown', onKey)
    window.addEventListener('scroll', onMove, true)
    window.addEventListener('resize', onMove)
    return () => {
      document.removeEventListener('mousedown', onDoc)
      document.removeEventListener('keydown', onKey)
      window.removeEventListener('scroll', onMove, true)
      window.removeEventListener('resize', onMove)
    }
  }, [open])
  // 量徽章鈕與浮層，算出視窗座標；在上屏前寫好，不閃到錯的位置（#1381）。
  useLayoutEffect(() => {
    if (!open || !btnRef.current || !popRef.current) return
    const b = btnRef.current.getBoundingClientRect()
    const p = popRef.current
    setPos(popPosition({ left: b.left, top: b.top, bottom: b.bottom }, { width: p.offsetWidth, height: p.offsetHeight }, window.innerWidth, window.innerHeight))
  }, [open])
  // 「關掉這則」之後：徽章還在就回到徽章鈕；不在了就交給事先記下的可聚焦祖先（找不到就不處理）。
  useEffect(() => {
    const pending = restoreFocus.current
    if (!pending) return
    restoreFocus.current = null
    if (btnRef.current) btnRef.current.focus()
    else if (pending.fallback?.isConnected) pending.fallback.focus()
  })
  if (!row) return null
  const scene = JUDGE_SCENES.find((s) => s.id === row.regex_verdict)?.name ?? row.regex_verdict
  const prob = probabilityText(row)
  const at = Date.parse(row.at)
  return (
    <span ref={wrapRef} className="judge-badge-wrap" onClick={(e) => e.stopPropagation()}>
      <button
        ref={btnRef}
        type="button"
        className="judge-badge"
        aria-expanded={open}
        aria-label={`Jev 第二意見：${scene}`}
        title={`Jev：${scene}（機率 ${prob}）`}
        onClick={() => setOpenId(open ? null : row.id)}
      >
        Jev
      </button>
      {open ? (
        <div
          ref={popRef}
          tabIndex={-1}
          className="judge-badge-pop"
          role="dialog"
          aria-label="Jev 第二意見"
          style={pos ? { position: 'fixed', left: pos.left, top: pos.top } : { position: 'fixed', visibility: 'hidden', left: 0, top: 0 }}
        >
          <p>
            <b>{scene}</b>・機率 {prob}
          </p>
          <p className="hint">{Number.isNaN(at) ? row.at : new Date(at).toLocaleString('zh-TW', { hour12: false })}</p>
          {row.matched_line ? <pre className="judge-badge-line">{row.matched_line}</pre> : null}
          <p className="hint">這只是第二意見，不會自動動作。</p>
          <button
            type="button"
            className="btn"
            onClick={() => {
              restoreFocus.current = { fallback: wrapRef.current?.parentElement?.closest<HTMLElement>('[tabindex]') ?? null }
              setOpenId(null)
              dismiss(row.id)
            }}
          >
            關掉這則
          </button>
        </div>
      ) : null}
    </span>
  )
}

/** 開畫面讀一次；之後每次 `supervisor_changed`（`supervisorRev` 變）重抓，節流在 store 裡（最短 60 秒）。 */
export function JudgeLedgerWatch() {
  const rev = useStore((s) => s.supervisorRev)
  useEffect(() => {
    void useJudgeLedger.getState().refresh()
  }, [rev])
  return null
}
