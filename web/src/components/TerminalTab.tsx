import { useCallback, useEffect, useMemo, useState } from 'react'
import { useScrollTail } from '../hooks/useScrollTail'
import { movePaneToTab } from '../api'
import type { TerminalSnapshot } from '../api/types'
import { useStore } from '../store/store'
import { linkifyTerm } from './TermLinks'
import { setTermWrap, useTermWrap } from './termWrap'
import './terminalTab.css'

/** Below this many columns a TUI agent's output is fragmented beyond repair (31-column grok pane, 2026-09-06). */
const READABLE_COLUMNS = 60

/** Collapse runs of blank rows. A narrow pane is mostly padding — 22 rows, 13 of them empty. */
function squeeze(text: string): string {
  return text.replace(/\n[ \t]*(?:\n[ \t]*)+/g, '\n\n')
}

/** SPEC §3.2: read-only `recent_unwrapped` snapshot with a manual refresh (no xterm.js). */
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

  const refresh = useCallback(async () => {
    setLoading(true)
    try {
      setSnap(await readTerminal(botId, 'recent_unwrapped', lines))
      setErr(null)
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }, [botId, lines, readTerminal])

  useEffect(() => {
    void refresh()
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
    if (err) return `讀取終端失敗：${err}`
    if (!snap) return '讀取中…'
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
        {/* 手機預設折行（390px 看不到 185 欄的右半邊），桌機預設不折；按過就記在 localStorage。 */}
        <label className="conn" title="折行後 TUI 畫的框線與對齊會跑掉，但整行讀得到；不折行則維持原樣，靠橫捲看右半邊。">
          <input type="checkbox" checked={wrap} onChange={(e) => setTermWrap(e.target.checked)} />
          換行
        </label>
        <span className="spacer" />
        <span className="hint term-bar-note">唯讀快照，按「刷新」更新</span>
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
      {/* 內容全部在一行：`<pre>` 會照實吐出換行與縮排，JSX 的排版不能溜進終端畫面。 */}
      <pre className={`term${wrap ? ' term-wrap' : ''}`} ref={tail.ref} onScroll={tail.onScroll}>{linkifyTerm(body, snap?.columns)}</pre>
    </div>
  )
}
