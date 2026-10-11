import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'

/**
 * 對話清單與終端快照共用的貼底捲動。`stick` 是 ref、帶不動 render，所以量測也寫進 state 給按鈕用。
 * 內容會在 layout effect 後才長高（圖片、字體、收合），所以加尺寸觀察：沒往上捲就重新貼底。
 * 人不在底部時前面插進東西（「載入更早的訊息」）要保住原本看的那一則（#1239）：記下第一則看得到的 `[data-msg-id]`
 * 與它離容器頂的距離，內容換過再把距離補回來。不靠瀏覽器的 scroll anchoring（Safari 沒有、Chrome 在 scrollTop 0 時不做）。
 */
export function useScrollTail<T extends HTMLElement = HTMLDivElement>(deps: unknown[]) {
  const ref = useRef<T>(null)
  const stick = useRef(true)
  const anchor = useRef<{ node: HTMLElement; offset: number } | null>(null)
  const [atBottom, setAtBottom] = useState(true)

  const pin = useCallback(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [])

  // 記下人現在看的第一則；貼底時不需要錨。
  const mark = useCallback(() => {
    const el = ref.current
    if (!el || stick.current) {
      anchor.current = null
      return
    }
    const top = el.getBoundingClientRect().top
    for (const node of el.querySelectorAll<HTMLElement>('[data-msg-id]')) {
      const r = node.getBoundingClientRect()
      if (r.bottom > top) {
        anchor.current = { node, offset: r.top - top }
        return
      }
    }
    anchor.current = null
  }, [])

  // Follow the tail (new messages, live output growing) only while the user is at the bottom.
  // 先還原錨（不在底部、錨還在），再貼底，最後記新錨。錨已經在原位時差為 0，不會補兩次。
  useLayoutEffect(() => {
    const el = ref.current
    const a = anchor.current
    if (el && !stick.current && a && a.node.isConnected) {
      const now = a.node.getBoundingClientRect().top - el.getBoundingClientRect().top
      if (Math.abs(now - a.offset) > 1) el.scrollTop += now - a.offset
    }
    pin()
    mark()
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
    mark()
    setAtBottom((prev) => (prev === near ? prev : near))
  }

  const toBottom = () => {
    const el = ref.current
    if (!el) return
    stick.current = true
    anchor.current = null
    // Jump, don't animate: `smooth` takes seconds on a long scrollback.
    el.scrollTop = el.scrollHeight
    setAtBottom(true)
  }

  return { ref, onScroll, atBottom, toBottom }
}
