import { useEffect } from 'react'
import { dialogOpen } from '../lib/dialogOpen'
import { isImeEnter } from '../lib/ime'
import { settingsBlocksLeave } from '../lib/settingsLeaveGuard'
import { useStore } from '../store/store'

export function useBotSwitchKeys() {
  const selectAdjacentBot = useStore((s) => s.selectAdjacentBot)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.altKey || e.metaKey || e.ctrlKey || e.shiftKey || isImeEnter(e)) return
      if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return
      if (e.defaultPrevented) return
      const target = e.target instanceof Element ? e.target : null
      if (target?.closest('.bot-row')) return
      if (dialogOpen()) return
      if (settingsBlocksLeave()) {
        e.preventDefault()
        return
      }
      e.preventDefault()
      selectAdjacentBot(e.key === 'ArrowUp' ? -1 : 1)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selectAdjacentBot])
}
