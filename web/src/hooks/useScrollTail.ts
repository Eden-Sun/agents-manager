import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'

/**
 * 「貼著最新的那一則」——對話清單與終端快照共用的捲動尾巴。
 *
 * `stick`（ref）決定清單要不要跟著新輸出走，但 ref 帶不動 render，所以同一份量測也寫進
 * state，「捲到最新」那顆按鈕才只在使用者真的離開底部時才出現。
 *
 * 只靠 `deps` 變化時捲一次是不夠的：一進對話要看到最新那則，可是「最新」的高度不是掛載
 * 當下就定案的——圖片解碼完、字體換掉、氣泡量完自己要不要收合、視窗或圖片暫存區改變面板
 * 高度，都會在 layout effect 跑完之後才把內容撐高，原本貼著的底部就浮到畫面外，於是一進去
 * 停在中間。所以再掛一組尺寸觀察：只要使用者還沒自己往上捲，任何一次長高都重新貼回底部。
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
    // 觀察容器自己（面板高度變了）以及每個直接子節點——一則訊息裡的圖片載入完是那則自己
    // 長高，觀察那一顆就夠了，不必往下鋪滿整棵樹。子節點會來來去去，用 MutationObserver
    // 把新來的補上（對已經觀察過的節點再 `observe` 是無害的）。
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
    // Jump, don't animate: a long scrollback makes `smooth` take seconds, and the point of
    // the button is to get there at once. The list keeps following the tail afterwards.
    el.scrollTop = el.scrollHeight
    setAtBottom(true)
  }

  return { ref, onScroll, atBottom, toBottom }
}
