import { useT } from '../lib/i18n'
import { useCodexTerminalFooter } from '../hooks/useCodexTerminalFooter'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useScrollTail } from '../hooks/useScrollTail'
import { movePaneToTab } from '../api'
import type { TerminalSnapshot } from '../api/types'
import { useStore } from '../store/store'
import { AnsiTerm } from './AnsiTerm'
import { setTermWrap, useTermWrap } from './termWrap'
import './terminalTab.css'
import { ESC } from '../lib/charCodes'

const READABLE_COLUMNS = 60

const AUTO_REFRESH_MS = 3000

const AUTO_REFRESH_WORKING_MS = 1000

const SQUEEZE_BLANK_ROWS = new RegExp(`\\n(?:[ \\t]|${ESC}\\[[0-9;:]*m)*(?:\\n(?:[ \\t]|${ESC}\\[[0-9;:]*m)*)+`, 'g')

function squeeze(text: string): string {
  return text.replace(SQUEEZE_BLANK_ROWS, '\n\n')
}

export function TerminalTab({ botId }: { botId: string }) {
  const t = useT()
  const readTerminal = useStore((s) => s.readTerminal)
  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [lines, setLines] = useState(200)
  const [tight, setTight] = useState(true)
  const [moving, setMoving] = useState(false)
  const [movedBot, setMovedBot] = useState<string | null>(null)
  const [moveErr, setMoveErr] = useState<{ botId: string; text: string } | null>(null)
  const wrap = useTermWrap()
  const working = useStore((s) => s.runs[botId]?.agent_status === 'working')

  const inFlight = useRef(false)
  const gen = useRef(0)
  const refresh = useCallback(async (quiet = false) => {
    if (quiet && inFlight.current) return
    const mine = ++gen.current
    inFlight.current = true
    if (!quiet) setLoading(true)
    try {
      const next = await readTerminal(botId, 'recent_unwrapped', lines, 'ansi')
      if (mine !== gen.current) return
      setSnap(next)
      setErr(null)
    } catch (e) {
      if (mine !== gen.current) return
      setErr(e instanceof Error ? e.message : String(e))
    } finally {
      if (mine === gen.current) {
        inFlight.current = false
        if (!quiet) setLoading(false)
      }
    }
  }, [botId, lines, readTerminal])

  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setSnap(null)
    setErr(null)
    setLoading(false)
  }

  useEffect(() => {
    void refresh()
  }, [refresh])

  const autoMs = working ? AUTO_REFRESH_WORKING_MS : AUTO_REFRESH_MS
  useEffect(() => {
    const id = window.setInterval(() => {
      if (!document.hidden) void refresh(true)
    }, autoMs)
    return () => window.clearInterval(id)
  }, [refresh, autoMs])

  const move = useCallback(async () => {
    setMoving(true)
    setMoveErr(null)
    try {
      await movePaneToTab(botId)
      setMovedBot(botId)
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
  const terminal = useCodexTerminalFooter(botId, snap)
  const body = useMemo(() => {
    if (!snap) return err ? t('讀取終端失敗：{err}', { err }) : t('讀取中…')
    return tight ? squeeze(terminal.body) : terminal.body
  }, [err, snap, tight, terminal.body, t])

  const tail = useScrollTail<HTMLPreElement>([body])

  return (
    <div className="term-pane">
      <div className="term-bar">
        <button type="button" className="btn" onClick={() => void refresh()} disabled={loading}>
          {loading ? t('刷新中…') : t('刷新')}
        </button>
        <label className="conn">
          {t('行數')}
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
            title={t('抓法 recent_unwrapped') + `${

              snap.revision !== null && snap.revision !== undefined ? `・revision ${snap.revision}` : ''
            }`}
          >
            pane <code>{snap.pane_id}</code>
            {snap.columns ? `・${snap.columns}×${snap.rows ?? '?'}` : ''}
          </span>
        ) : null}
        {snap?.truncated ? (
          <span
            className="hint term-truncated-chip"
            title={t('輸出超過 {n} 行，僅顯示最新 {n} 行', { n: lines })}
          >
            {t('（超過 {n} 行已截斷）', { n: lines })}
          </span>
        ) : null}
        <label className="conn">
          <input type="checkbox" checked={tight} onChange={(e) => setTight(e.target.checked)} />
          {t('壓縮空行')}
        </label>
        {}
        <label className="conn" title={t('折行後 TUI 畫的框線與對齊會跑掉，但整行讀得到；不折行則維持原樣，靠橫捲看右半邊。')}>
          <input type="checkbox" checked={wrap} onChange={(e) => setTermWrap(e.target.checked)} />
          {t('換行')}
        </label>
        <span className="spacer" />
        <span className="hint term-bar-note">{t('唯讀快照，工作中每秒、閒置每 3 秒自動更新')}</span>
      </div>
      {narrow ? (
        <div className="term-warn" role="status">
          {t('這個 pane 只有 {cols} 欄，agent 的輸出在終端就被折成碎片，任何解析都還原不回來。同一個分頁裡的 pane 互搶寬度，把這個 bot 移到自己的分頁就能獨佔整個 workspace 的寬度（拉寬只是把窄的問題推給鄰居，放大也沒用——終端的字元格線是固定的）。hook 取得的回覆不受影響。', { cols: snap?.columns ?? 0 })}
          <div className="term-warn-actions">
            <button type="button" className="btn" onClick={() => void move()} disabled={moving}>
              {moving ? t('移動中…') : t('移到自己的分頁')}
            </button>
            <span className="term-warn-caveat">
              {t('搬的是現有的 pane：不會重啟 bot，也不會中斷正在跑的回合。只影響之後的輸出——上面已經被終端折斷的內容是 scrollback 裡的原文，救不回來。')}
            </span>
          </div>
          {moveErrText ? <div className="term-warn-caveat">{t('搬移失敗：{msg}', { msg: moveErrText })}</div> : null}
          {moved ? (
            <div className="term-warn-caveat">
              {t('已送出搬移，但這個 pane 還是只有 {cols} 欄——可能 herdr 還沒套用，按「刷新」再看一次。', { cols: snap?.columns ?? 0 })}
            </div>
          ) : null}
        </div>
      ) : moved ? (
        <div className="term-warn is-done" role="status">
          {}
          {snap?.columns
            ? t('已把這個 pane 移到自己的分頁，現在有 {cols} 欄；bot 沒有重啟，回合也沒中斷。', { cols: snap.columns })
            : t('已把這個 pane 移到自己的分頁；bot 沒有重啟，回合也沒中斷。')}
          <div className="term-warn-actions">
            <button type="button" className="btn" onClick={() => setMovedBot(null)}>
              {t('知道了')}
            </button>
            <span className="term-warn-caveat">
              {t('只影響之後的輸出：上面那些碎片是終端 scrollback 裡已經折斷的原文，搬分頁救不回來，等 agent 印出新內容才會是完整寬度。')}
            </span>
          </div>
        </div>
      ) : null}
      {err && snap ? (
        <div className="term-warn" role="status">
          {t('讀取終端失敗：{err}（下面是上一次成功讀到的畫面，會自動重試）', { err })}
        </div>
      ) : null}
      {}
      <pre className={`term${wrap ? ' term-wrap' : ''}`} ref={tail.ref} onScroll={tail.onScroll}><AnsiTerm text={body} columns={snap?.columns} /></pre>
    </div>
  )
}
