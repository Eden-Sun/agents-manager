import { useEffect, useState } from 'react'
import type { TerminalSnapshot, TerminalSource } from '../api/types'
import { useStore } from '../store/store'

export interface TerminalPoll {
  snap: TerminalSnapshot | null
  err: string | null
  /** 立刻重讀一次（送完按鍵想馬上看到 TUI 的反應）。 */
  refresh: () => void
}

/**
 * SPEC §3.2 的終端輪詢：blocked 面板與彈出的全畫面終端共用同一份，免得兩邊各寫一次
 * setTimeout 迴圈之後參數（來源、行數、節奏）悄悄漂開。
 *
 * `paused` 是給「同一個 bot 有兩個檢視同時掛著」用的：全畫面終端一開，底下那個小面板就
 * 停掉自己的輪詢，一個 bot 永遠只有一條 `GET /terminal` 在跑。
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
  const [snap, setSnap] = useState<TerminalSnapshot | null>(null)
  const [err, setErr] = useState<string | null>(null)
  /** 改變它就重跑下面的 effect：等於「取消現在排著的那次，立刻讀一次」。 */
  const [nonce, setNonce] = useState(0)

  // 換 bot 時把畫面清掉是 render 當下就該有的結果，不是一個 effect：留著上一個 bot 的終端
  // 內容不只是舊資料，是「另一台機器的畫面」，一眼看過去會以為是這個 bot 的。
  const [lastBot, setLastBot] = useState(botId)
  if (lastBot !== botId) {
    setLastBot(botId)
    setSnap(null)
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
