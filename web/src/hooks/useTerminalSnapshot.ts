import { useEffect, useState } from 'react'
import type { TerminalSnapshot, TerminalSource } from '../api/types'
import { useStore } from '../store/store'
import { peekPrefetched } from '../lib/blockedPrefetch'

export interface TerminalPoll {
  snap: TerminalSnapshot | null
  err: string | null
  /** 立刻重讀一次（送完按鍵想馬上看到 TUI 的反應）。 */
  refresh: () => void
}

/**
 * SPEC §3.2 終端輪詢，blocked 面板與全畫面終端共用以免參數漂開。
 * `paused`：全畫面一開小面板就停，一個 bot 只有一條 `GET /terminal`。
 */
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
  // blocked 的 bot 在背景先讀過（`lib/blockedPrefetch.ts`）：視窗一打開就有畫面，不必等第一次輪詢。
  const [snap, setSnap] = useState<TerminalSnapshot | null>(() => peekPrefetched(botId, source, lines))
  const [err, setErr] = useState<string | null>(null)
  const [nonce, setNonce] = useState(0)

  // 換 bot 在 render 當下清畫面（不用 effect）：舊畫面會被誤認成這個 bot 的。
  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setSnap(peekPrefetched(botId, source, lines))
    setErr(null)
  }

  useEffect(() => {
    if (paused) return
    let alive = true
    let timer: ReturnType<typeof setTimeout> | null = null
    const tick = async () => {
      try {
        const s = await readTerminal(botId, source, lines)
        if (alive) {
          setSnap(s)
          setErr(null)
        }
      } catch (e) {
        if (alive) setErr(e instanceof Error ? e.message : String(e))
      }
      if (alive) timer = setTimeout(() => void tick(), intervalMs)
    }
    void tick()
    return () => {
      alive = false
      if (timer) clearTimeout(timer)
    }
  }, [botId, source, lines, intervalMs, paused, readTerminal, nonce])

  return { snap, err, refresh: () => setNonce((n) => n + 1) }
}
