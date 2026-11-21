import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'

export function useScrollTail<T extends HTMLElement = HTMLDivElement>(deps: unknown[]) {
  const ref = useRef<T>(null)
  const stick = useRef(true)
  const anchor = useRef<{ node: HTMLElement; offset: number } | null>(null)
  const [atBottom, setAtBottom] = useState(true)

  const pin = useCallback(() => {
    const el = ref.current
    if (el && stick.current) el.scrollTop = el.scrollHeight
  }, [])

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

  useLayoutEffect(() => {
    const el = ref.current
    const a = anchor.current
    if (el && !stick.current && a && a.node.isConnected) {
      const now = a.node.getBoundingClientRect().top - el.getBoundingClientRect().top
      if (Math.abs(now - a.offset) > 1) el.scrollTop += now - a.offset
    }
    pin()
    mark()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)

  useEffect(() => {
    const el = ref.current
    if (!el || typeof ResizeObserver === 'undefined') return
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
    el.scrollTop = el.scrollHeight
    setAtBottom(true)
  }

  return { ref, onScroll, atBottom, toBottom }
}
