import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'

/**
 * 對話清單與終端快照共用的貼底捲動。`stick` 是 ref、帶不動 render，所以量測也寫進 state 給按鈕用。
 * 內容會在 layout effect 後才長高（圖片、字體、收合），所以加尺寸觀察：沒往上捲就重新貼底。
 */
export function useScrollTail<T extends HTMLElement = HTMLDivElement>(deps: unknown[]) {
  const ref = useRef<T>(null)
  const stick = useRef(true)
  const [atBottom, setAtBottom] = useState(true)

  const pin = useCallback(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [])

  // Follow the tail (new messages, live output growing) only while the user is at the bottom.
  useLayoutEffect(() => {
    pin()
    // The list this hook serves decides what "changed" means.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)

  useEffect(() => {
    const el = ref.current
    if (!el || typeof ResizeObserver === 'undefined') return
    // 觀察容器與直接子節點就夠（圖片載入是那則自己長高）；新子節點靠 MutationObserver 補，重複 observe 無害。
    const ro = new ResizeObserver(pin)
    const watch = () => {
      ro.observe(el)
      for (const child of Array.from(el.children)) ro.observe(child)
    }
    watch()
    const mo = new MutationObserver(watch)
    mo.observe(el, { childList: true })
    return () => {
      ro.disconnect()
      mo.disconnect()
    }
  }, [pin])

  const onScroll = (e: { currentTarget: T }) => {
    const el = e.currentTarget
    const near = el.scrollHeight - el.scrollTop - el.clientHeight < 80
    stick.current = near
    setAtBottom((prev) => (prev === near ? prev : near))
  }

  const toBottom = () => {
    const el = ref.current
    if (!el) return
    stick.current = true
    // Jump, don't animate: `smooth` takes seconds on a long scrollback.
    el.scrollTop = el.scrollHeight
    setAtBottom(true)
  }

  return { ref, onScroll, atBottom, toBottom }
}
