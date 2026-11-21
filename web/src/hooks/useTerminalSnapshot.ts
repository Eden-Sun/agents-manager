import { useEffect, useState } from 'react'
import type { TerminalSnapshot, TerminalSource } from '../api/types'
import { useStore } from '../store/store'
import { peekPrefetched } from '../lib/blockedPrefetch'

export interface TerminalPoll {
  snap: TerminalSnapshot | null
  err: string | null
  refresh: () => void
}

export function useTerminalSnapshot(
  botId: string,
  {
    source = 'visible',
    lines = 200,
    intervalMs = 1000,
    paused = false,
  }: { source?: TerminalSource; lines?: number; intervalMs?: number; paused?: boolean } = {},
): TerminalPoll {
  const readTerminal = useStore((s) => s.readTerminal)
  const [snap, setSnap] = useState<TerminalSnapshot | null>(() => peekPrefetched(botId, source, lines))
  const [err, setErr] = useState<string | null>(null)
  const [nonce, setNonce] = useState(0)

  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setSnap(peekPrefetched(botId, source, lines))
    setErr(null)
  }

  useEffect(() => {
    if (paused) return
    let alive = true
    let busy = false
    let timer: ReturnType<typeof setTimeout> | null = null
    const next = () => {
      if (alive) timer = setTimeout(() => void tick(), intervalMs)
    }
    const tick = async () => {
      if (document.visibilityState === 'hidden') {
        next()
        return
      }
      busy = true
      try {
        const s = await readTerminal(botId, source, lines)
        if (alive) {
          setSnap(s)
          setErr(null)
        }
      } catch (e) {
        if (alive) setErr(e instanceof Error ? e.message : String(e))
      }
      busy = false
      next()
    }
    const onVisible = () => {
      if (document.visibilityState !== 'visible' || busy) return
      if (timer) clearTimeout(timer)
      timer = null
      void tick()
    }
    document.addEventListener('visibilitychange', onVisible)
    void tick()
    return () => {
      alive = false
      document.removeEventListener('visibilitychange', onVisible)
      if (timer) clearTimeout(timer)
    }
  }, [botId, source, lines, intervalMs, paused, readTerminal, nonce])

  return { snap, err, refresh: () => setNonce((n) => n + 1) }
}
