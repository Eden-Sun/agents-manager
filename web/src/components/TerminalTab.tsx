import { useCallback, useEffect, useMemo, useState } from 'react'
import { isPaneMoveUnsupported, movePaneToTab } from '../api'
import type { TerminalSnapshot } from '../api/types'
import { useStore } from '../store/store'

/**
 * Below this many columns a TUI agent lays its own text out a fragment per row and the spaces
 * fall off the ends, so the snapshot cannot be read however it is rendered. Observed on a
 * 31-column grok pane (2026-09-06), whose widest row held four characters.
 */
const READABLE_COLUMNS = 60

/**
 * 「這版 daemon 沒有 `pane/move-to-tab`」是 daemon 的能力，不是這個 bot 的狀態，問一次就夠了。
 * 放在 module scope：切 bot / 切分頁讓元件重新掛載時不會再打一次已知會 405 的請求，
 * 按鈕也就從此不再出現（docs/FRONTEND.md §8：缺端點要靜默退回，不是每次都噴錯）。
 */
let paneMoveUnsupported = false

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
  /**
   * 搬移結果記的是「哪一個 bot」而不是布林：換 bot 時這個元件不一定重新掛載，記 bot id 才能
   * 在 render 當下就算出來，不用一個把上一個 bot 的結果清掉的 effect。
   */
  const [movedBot, setMovedBot] = useState<string | null>(null)
  const [moveErr, setMoveErr] = useState<{ botId: string; text: string } | null>(null)
  const [noMove, setNoMove] = useState(paneMoveUnsupported)

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
      if (isPaneMoveUnsupported(e)) {
        // 刻意不 notify()：這是「daemon 還沒跟上」，不是使用者做錯了什麼。
        paneMoveUnsupported = true
        setNoMove(true)
      } else {
        setMoveErr({ botId, text: e instanceof Error ? e.message : String(e) })
      }
    } finally {
      setMoving(false)
    }
  }, [botId, refresh])

  const narrow = snap?.columns != null && snap.columns < READABLE_COLUMNS
  /** 這個 bot 在這次掛載裡搬過了。留著是為了讓「只影響之後的輸出」在警示消失後還講得完。 */
  const moved = movedBot === botId
  const moveErrText = moveErr?.botId === botId ? moveErr.text : null
  const body = useMemo(() => {
    if (err) return `讀取終端失敗：${err}`
    if (!snap) return '讀取中…'
    return tight ? squeeze(snap.text) : snap.text
  }, [err, snap, tight])

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
        <span>
          source=<code>recent_unwrapped</code>
          {snap?.revision !== null && snap?.revision !== undefined ? `・revision ${snap.revision}` : ''}
          {snap?.truncated ? '・已截斷' : ''}
        </span>
        {snap?.pane_id ? (
          <span className="hint">
            pane <code>{snap.pane_id}</code>
            {snap.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
          </span>
        ) : null}
        <label className="conn">
          <input type="checkbox" checked={tight} onChange={(e) => setTight(e.target.checked)} />
          壓縮空行
        </label>
        <span className="spacer" />
        <span className="hint">唯讀快照，第一階段不做 xterm.js 串流</span>
      </div>
      {narrow ? (
        <div className="term-warn" role="status">
          這個 pane 只有 {snap?.columns} 欄，agent 的輸出在終端就被折成碎片，任何解析都還原不回來。
          同一個分頁裡的 pane 互搶寬度，把這個 bot 移到自己的分頁就能獨佔整個 workspace 的寬度
          （拉寬只是把窄的問題推給鄰居，放大也沒用——終端的字元格線是固定的）。
          hook 取得的回覆不受影響。
          <div className="term-warn-actions">
            {noMove ? null : (
              <button type="button" className="btn" onClick={() => void move()} disabled={moving}>
                {moving ? '移動中…' : '移到自己的分頁'}
              </button>
            )}
            <span className="term-warn-caveat">
              {noMove
                ? '這版 daemon 還沒有自動搬移，請在 herdr 手動把這個 pane 移到新分頁（pane.move → new_tab）。搬 pane 不會重啟 bot，但一樣只影響之後的輸出。'
                : '搬的是現有的 pane：不會重啟 bot，也不會中斷正在跑的回合。只影響之後的輸出——上面已經被終端折斷的內容是 scrollback 裡的原文，救不回來。'}
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
      <pre className="term">{body}</pre>
    </div>
  )
}
