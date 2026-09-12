import { useCallback } from 'react'

/**
 * 需要 `preventDefault` 的滾輪處理器要用原生、非 passive 的 listener 掛。
 *
 * React 17+ 把 `onWheel` 註冊成 passive listener，裡面的 `e.preventDefault()` 無效還噴
 * "Unable to preventDefault inside passive event listener"——「這一列橫捲、到底才把滾動還給整頁」
 * 的分流因此失效，列橫捲的同時整頁也跟著直捲（docs/reviews/2026-09-12/web.md §4 passive wheel）。
 *
 * 回傳的是 ref callback（React 19 的 ref cleanup），同一個 callback 可以掛在 map 出來的好幾個節點上，
 * 每個節點各自加、各自拆。
 */
export function useWheelRef<T extends HTMLElement>(handler: (e: WheelEvent) => void): (el: T | null) => void | (() => void) {
  return useCallback(
    (el: T | null) => {
      if (!el) return
      el.addEventListener('wheel', handler, { passive: false })
      return () => el.removeEventListener('wheel', handler)
    },
    [handler],
  )
}
