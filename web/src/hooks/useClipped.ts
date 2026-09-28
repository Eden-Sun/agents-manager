import { useLayoutEffect, useState } from 'react'
import type { RefObject } from 'react'

/**
 * `outer` 被 flex 壓得比 `inner` 窄（內容被裁掉）時回 true。給標題列裡會讓位的徽章用：裁成一小截不如整顆藏起來。
 * 呼叫端拿它切 `visibility`，不改排版，所以不會量完又變寬、來回跳。
 */
export function useClipped(outer: RefObject<HTMLElement | null>, inner: RefObject<HTMLElement | null>): boolean {
  const [clipped, setClipped] = useState(false)
  useLayoutEffect(() => {
    const o = outer.current
    const i = inner.current
    if (!o || !i) return
    const check = () => setClipped(o.getBoundingClientRect().width + 0.5 < i.getBoundingClientRect().width)
    check()
    if (typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(check)
    ro.observe(o)
    ro.observe(i)
    return () => ro.disconnect()
  }, [outer, inner])
  return clipped
}
