import { useEffect, useLayoutEffect, useRef } from 'react'

export function usePollWhileVisible(tick: () => void, ms: number | null): void {
  const latest = useRef(tick)
  useLayoutEffect(() => {
    latest.current = tick
  })
  useEffect(() => {
    if (ms === null) return
    const run = () => {
      if (document.visibilityState === 'visible') latest.current()
    }
    const onVisible = () => {
      if (document.visibilityState === 'visible') latest.current()
    }
    const id = setInterval(run, ms)
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      clearInterval(id)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [ms])
}
