import { useEffect, useRef } from 'react'
import { useStore } from '../store/store'

/**
 * 同一顆 turn 的即時狀態：occurrence 在 turn 開始後仍是 queued，所以 turn 的 status 變化（turn_updated）要直接讀
 * store 的 turns，不能只靠 schedules_changed（那只在終態才由 daemon 推）。只有自己綁的 turn 變動才觸發這一列重抓。
 */
export function useLiveTurnStatus(botId: string | undefined, turnId: string | null | undefined): string | undefined {
  return useStore((st) => (botId && turnId ? st.turns[botId]?.[turnId]?.status : undefined))
}

/** 這一列綁的 turn 狀態變了（queued→in_flight→終態），就請上層重抓，讓 open 與 occurrence 狀態跟上。 */
export function useOnTurnChange(liveStatus: string | undefined, onChange: () => void) {
  const prev = useRef(liveStatus)
  useEffect(() => {
    if (prev.current === liveStatus) return
    prev.current = liveStatus
    onChange()
  }, [liveStatus, onChange])
}
