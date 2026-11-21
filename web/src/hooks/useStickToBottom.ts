import { useCallback, useLayoutEffect, useRef, type RefObject, type UIEvent } from 'react'

const NEAR_BOTTOM_PX = 40

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
