import { useEffect, useLayoutEffect, useRef } from 'react'

/**
 * 固定週期輪詢，但分頁在背景就不跑（每一拍都是一次 daemon 請求，背景分頁開著卻一直打是白浪費）。
 * 回到前景時立刻補一次，不等下一拍。`tick` 每次 render 都可能換身分，用 ref 讀最新的，不重排計時器。
 */
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
