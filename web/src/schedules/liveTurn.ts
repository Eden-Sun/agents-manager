import { useEffect, useRef } from 'react'
import { useStore } from '../store/store'

export function useLiveTurnStatus(botId: string | undefined, turnId: string | null | undefined): string | undefined {
  return useStore((st) => (botId && turnId ? st.turns[botId]?.[turnId]?.status : undefined))
}

export function useOnTurnChange(liveStatus: string | undefined, onChange: () => void) {
  const prev = useRef(liveStatus)
  useEffect(() => {
    if (prev.current === liveStatus) return
    prev.current = liveStatus
    onChange()
  }, [liveStatus, onChange])
}
