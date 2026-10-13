import { useEffect, useMemo, useState } from 'react'
import type { TerminalSnapshot } from '../api/types'
import { splitCodexTerminalFooter, type CodexTerminalFooter } from '../lib/codexTerminalFooter'
import { useStore, type StoreState } from '../store/store'

export function terminalFooterFor(s: StoreState, botId: string | null): CodexTerminalFooter | null {
  const view = s.terminalFooter
  return botId && view?.botId === botId && view.runId === (s.runs[botId]?.id ?? null) ? view.footer : null
}

/** Publish only an accepted, currently displayed snapshot; API text remains unchanged. */
export function useCodexTerminalFooter(botId: string, snap: TerminalSnapshot | null) {
  const kind = useStore((s) => s.bots.find((b) => b.id === botId)?.kind)
  const runId = useStore((s) => s.runs[botId]?.id ?? null)
  // A run change can precede the next HTTP response. Do not relabel an old snapshot.
  const [seen, setSeen] = useState({ snap, runId })
  if (seen.snap !== snap) setSeen({ snap, runId })
  const current = snap?.run_id !== undefined ? snap.run_id === runId : seen.snap !== snap || seen.runId === runId
  const text = snap?.text ?? ''
  const parsed = useMemo(() => kind === 'codex' ? splitCodexTerminalFooter(text) : { body: text, footer: null }, [kind, text])
  const footer = current ? parsed.footer : null
  useEffect(() => {
    const view = { botId, runId, footer }
    useStore.setState({ terminalFooter: view })
    return () => {
      if (useStore.getState().terminalFooter === view) useStore.setState({ terminalFooter: null })
    }
  }, [botId, runId, footer])
  return { body: parsed.body, footer }
}
