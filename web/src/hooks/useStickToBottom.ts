import { useCallback, useLayoutEffect, useRef, type RefObject, type UIEvent } from 'react'

/** 離底部這麼近（px）還算「在底部」：差一兩列不算使用者往上捲了。 */
const NEAR_BOTTOM_PX = 40

/**
 * 終端快照的 `<pre>` 跟著畫面底部：提示（選項、確認框）永遠在畫面最下面，捲軸停在頂端只會看到上面的工具輸出
 * （2026-11-05 使用者截圖：面板停在 `Bash(…)` 與 JSON，選項在看不到的下面）。
 *
 * 每次 `text` 變了（每秒一張新快照）且使用者**沒有往上捲**就捲到底；往上捲過就不再動（捲回底部附近就恢復跟隨）。
 * 回傳的 `onScroll` 掛在同一個元素上。`enabled=false` 時什麼都不做。
 */
export function useStickToBottom(
  ref: RefObject<HTMLElement | null>,
  text: string | undefined,
  enabled = true,
): { onScroll: (e: UIEvent<HTMLElement>) => void } {
  const stuck = useRef(true)
  const onScroll = useCallback((e: UIEvent<HTMLElement>) => {
    const el = e.currentTarget
    stuck.current = el.scrollHeight - el.scrollTop - el.clientHeight <= NEAR_BOTTOM_PX
  }, [])
  useLayoutEffect(() => {
    const el = ref.current
    if (!enabled || !el || !text || !stuck.current) return
    el.scrollTop = el.scrollHeight
  }, [ref, text, enabled])
  return { onScroll }
}
