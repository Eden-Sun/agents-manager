import { useCallback, useEffect, useState } from 'react'
import type { TerminalSnapshot } from '../api/types'
import { useStore } from '../store/store'

/** SPEC §3.2: read-only `recent_unwrapped` snapshot with a manual refresh (no xterm.js). */
export function TerminalTab({ botId }: { botId: string }) {
  const readTerminal = useStore((s) => s.readTerminal)
  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [lines, setLines] = useState(200)

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
        <span className="spacer" />
        <span className="hint">唯讀快照，第一階段不做 xterm.js 串流</span>
      </div>
      <pre className="term">{err ? `讀取終端失敗：${err}` : (snap?.text ?? '讀取中…')}</pre>
    </div>
  )
}
