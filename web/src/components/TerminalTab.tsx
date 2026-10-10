import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useScrollTail } from '../hooks/useScrollTail'
import { movePaneToTab } from '../api'
import type { TerminalSnapshot } from '../api/types'
import { useStore } from '../store/store'
import { linkifyTerm } from './termLinks'
import { setTermWrap, useTermWrap } from './termWrap'
import './terminalTab.css'

/** Below this many columns a TUI agent's output is fragmented beyond repair (31-column grok pane, 2026-09-06). */
const READABLE_COLUMNS = 60

/** 自動刷新間隔（2026-09-28 使用者）。分頁在背景（`document.hidden`）時不抓。 */
const AUTO_REFRESH_MS = 3000

/** Collapse runs of blank rows. A narrow pane is mostly padding — 22 rows, 13 of them empty. */
function squeeze(text: string): string {
  return text.replace(/\n[ \t]*(?:\n[ \t]*)+/g, '\n\n')
}

/** SPEC §3.2: read-only `recent_unwrapped` snapshot, auto-refreshed every 3s plus a manual button (no xterm.js). */
export function TerminalTab({ botId }: { botId: string }) {
  const readTerminal = useStore((s) => s.readTerminal)
  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [lines, setLines] = useState(200)
  const [tight, setTight] = useState(true)
  const [moving, setMoving] = useState(false)
  /** 記 bot id 而非布林：換 bot 不一定重掛載，這樣不用 effect 清掉上一個 bot 的結果。 */
  const [movedBot, setMovedBot] = useState<string | null>(null)
  const [moveErr, setMoveErr] = useState<{ botId: string; text: string } | null>(null)
  const wrap = useTermWrap()

  /** 一次只抓一份：上一趟還沒回來時，自動那趟就跳過，不疊請求。 */
  const inFlight = useRef(false)
  /**
   * 請求世代（#918）：每趟 `refresh` 領一個號碼，回來時號碼不是最新的就整趟丟掉。換 bot／改行數會重跑 effect，
   * 它第一件事就是一趟非 quiet 的 `refresh()`，所以舊的在途回應自然作廢（不需要 cleanup 裡動 ref）。
   */
  const gen = useRef(0)
  /** `quiet`：自動刷新不切「刷新中…」，按鈕不會每 3 秒閃一次。 */
  const refresh = useCallback(async (quiet = false) => {
    if (quiet && inFlight.current) return
    const mine = ++gen.current
    inFlight.current = true
    if (!quiet) setLoading(true)
    try {
      const next = await readTerminal(botId, 'recent_unwrapped', lines)
      if (mine !== gen.current) return
      setSnap(next)
      setErr(null)
    } catch (e) {
      if (mine !== gen.current) return
      setErr(e instanceof Error ? e.message : String(e))
    } finally {
      // 被取代的那趟不碰旗標與「刷新中」：新的那趟（或 effect 的 cleanup）會管。
      if (mine === gen.current) {
        inFlight.current = false
        if (!quiet) setLoading(false)
      }
    }
  }, [botId, lines, readTerminal])

  // 換 bot 在 render 當下清畫面（同 `useTerminalSnapshot`，不用 effect）：上一顆的快照與錯誤不能留到下一趟回來。
  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setSnap(null)
    setErr(null)
    setLoading(false)
  }

  useEffect(() => {
    void refresh()
    const id = window.setInterval(() => {
      if (!document.hidden) void refresh(true)
    }, AUTO_REFRESH_MS)
    return () => window.clearInterval(id)
  }, [refresh])

  const move = useCallback(async () => {
    setMoving(true)
    setMoveErr(null)
    try {
      await movePaneToTab(botId)
      setMovedBot(botId)
      // 搬完立刻重讀：columns 會從幾十欄跳回整個 workspace 的寬度，警示自己就消失了。
      await refresh()
    } catch (e) {
      setMoveErr({ botId, text: e instanceof Error ? e.message : String(e) })
    } finally {
      setMoving(false)
    }
  }, [botId, refresh])

  const narrow = snap?.columns != null && snap.columns < READABLE_COLUMNS
  const moved = movedBot === botId
  const moveErrText = moveErr?.botId === botId ? moveErr.text : null
  const body = useMemo(() => {
    if (!snap) return err ? `讀取終端失敗：${err}` : '讀取中…'
    return tight ? squeeze(snap.text) : snap.text
  }, [err, snap, tight])

  // 貼底顯示最新輸出，除非使用者自己往上捲（`useScrollTail`）。
  const tail = useScrollTail<HTMLPreElement>([body])

  return (
    <div className="term-pane">
      <div className="term-bar">
        <button type="button" className="btn" onClick={() => void refresh()} disabled={loading}>
          {loading ? '刷新中…' : '刷新'}
        </button>
        <label className="conn">
          行數
          <select value={lines} onChange={(e) => setLines(Number(e.target.value))}>
            {[50, 100, 200, 500].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>
        {snap?.pane_id ? (
          <span
            className="hint term-pane-chip"
            title={`抓法 recent_unwrapped${
              snap.revision !== null && snap.revision !== undefined ? `・revision ${snap.revision}` : ''
            }`}
          >
            pane <code>{snap.pane_id}</code>
            {snap.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
          </span>
        ) : null}
        {snap?.truncated ? <span className="hint">已截斷</span> : null}
        <label className="conn">
          <input type="checkbox" checked={tight} onChange={(e) => setTight(e.target.checked)} />
          壓縮空行
        </label>
        {/* 預設折行（2026-09-28 使用者，桌機也折）；按過就記在 localStorage。 */}
        <label className="conn" title="折行後 TUI 畫的框線與對齊會跑掉，但整行讀得到；不折行則維持原樣，靠橫捲看右半邊。">
          <input type="checkbox" checked={wrap} onChange={(e) => setTermWrap(e.target.checked)} />
          換行
        </label>
        <span className="spacer" />
        <span className="hint term-bar-note">唯讀快照，每 3 秒自動更新</span>
      </div>
      {narrow ? (
        <div className="term-warn" role="status">
          這個 pane 只有 {snap?.columns} 欄，agent 的輸出在終端就被折成碎片，任何解析都還原不回來。
          同一個分頁裡的 pane 互搶寬度，把這個 bot 移到自己的分頁就能獨佔整個 workspace 的寬度
          （拉寬只是把窄的問題推給鄰居，放大也沒用——終端的字元格線是固定的）。
          hook 取得的回覆不受影響。
          <div className="term-warn-actions">
            <button type="button" className="btn" onClick={() => void move()} disabled={moving}>
              {moving ? '移動中…' : '移到自己的分頁'}
            </button>
            <span className="term-warn-caveat">
              搬的是現有的 pane：不會重啟 bot，也不會中斷正在跑的回合。只影響之後的輸出——上面已經被終端折斷的內容是 scrollback 裡的原文，救不回來。
            </span>
          </div>
          {moveErrText ? <div className="term-warn-caveat">搬移失敗：{moveErrText}</div> : null}
          {moved ? (
            <div className="term-warn-caveat">
              已送出搬移，但這個 pane 還是只有 {snap?.columns} 欄——可能 herdr 還沒套用，按「刷新」再看一次。
            </div>
          ) : null}
        </div>
      ) : moved ? (
        <div className="term-warn is-done" role="status">
          {/* 搬完之後 daemon 有時回不出 columns（實測 2026-09-06），寬度就別硬掰。 */}
          已把這個 pane 移到自己的分頁{snap?.columns ? `，現在有 ${snap.columns} 欄` : ''}；bot 沒有重啟，回合也沒中斷。
          <div className="term-warn-actions">
            <button type="button" className="btn" onClick={() => setMovedBot(null)}>
              知道了
            </button>
            <span className="term-warn-caveat">
              只影響之後的輸出：上面那些碎片是終端 scrollback 裡已經折斷的原文，搬分頁救不回來，
              等 agent 印出新內容才會是完整寬度。
            </span>
          </div>
        </div>
      ) : null}
      {err && snap ? (
        <div className="term-warn" role="status">
          讀取終端失敗：{err}（下面是上一次成功讀到的畫面，會自動重試）
        </div>
      ) : null}
      {/* 內容全部在一行：`<pre>` 會照實吐出換行與縮排，JSX 的排版不能溜進終端畫面。 */}
      <pre className={`term${wrap ? ' term-wrap' : ''}`} ref={tail.ref} onScroll={tail.onScroll}>{linkifyTerm(body, snap?.columns)}</pre>
    </div>
  )
}
