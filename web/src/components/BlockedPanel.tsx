import { useEffect, useRef, useState } from 'react'
import type { TerminalSnapshot } from '../api/types'
import { useStore } from '../store/store'

/**
 * Keys are forwarded verbatim to herdr `agent.send_keys` (the daemon does not translate
 * them), so these strings match what `lifecycle.rs` already sends: `esc`, `ctrl+c`.
 */
const KEYS: { label: string; keys: string[]; title: string }[] = [
  { label: 'Enter', keys: ['enter'], title: '送出 Enter' },
  { label: 'Esc', keys: ['esc'], title: '送出 Esc' },
  { label: 'y', keys: ['y'], title: '回答 y' },
  { label: 'n', keys: ['n'], title: '回答 n' },
  { label: '↑', keys: ['up'], title: '游標上移' },
  { label: '↓', keys: ['down'], title: '游標下移' },
  { label: 'ctrl+c', keys: ['ctrl+c'], title: '送出 ctrl+c' },
]

/** SPEC §3.2: while the agent is `blocked`, poll the `visible` snapshot once a second. */
export function BlockedPanel({ botId }: { botId: string }) {
  const readTerminal = useStore((s) => s.readTerminal)
  const sendKeys = useStore((s) => s.sendKeys)
  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  const alive = useRef(true)

  useEffect(() => {
    alive.current = true
    let timer: ReturnType<typeof setTimeout> | null = null
    const tick = async () => {
      try {
        const s = await readTerminal(botId, 'visible', 40)
        if (alive.current) {
          setSnap(s)
          setErr(null)
        }
      } catch (e) {
        if (alive.current) setErr(e instanceof Error ? e.message : String(e))
      }
      if (alive.current) timer = setTimeout(() => void tick(), 1000)
    }
    void tick()
    return () => {
      alive.current = false
      if (timer) clearTimeout(timer)
    }
  }, [botId, readTerminal])

  return (
    <section className="blocked" aria-label="終端等待回應">
      <div className="blocked-head">
        <span className="blocked-title">● agent 需要回應</span>
        <span className="blocked-sub">
          終端 <code>visible</code> 快照，每秒更新
          {snap?.revision !== null && snap?.revision !== undefined ? `（revision ${snap.revision}）` : ''}
          {snap?.truncated ? '・已截斷' : ''}
        </span>
      </div>
      <pre className="term blocked-term">{err ? `讀取終端失敗：${err}` : (snap?.text ?? '讀取中…')}</pre>
      <div className="keypad">
        {KEYS.map((k) => (
          <button
            key={k.label}
            type="button"
            className="key-btn"
            title={k.title}
            onClick={() => void sendKeys(botId, k.keys)}
          >
            {k.label}
          </button>
        ))}
        <span className="hint">按鍵會帶 expect_run_id，Run 不符時後端回 409</span>
      </div>
    </section>
  )
}
