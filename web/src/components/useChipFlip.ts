import { useLayoutEffect, useRef } from 'react'
import type { RefObject } from 'react'
import { orderChanged } from '../lib/chipOverflow'

export function useChipFlip(root: RefObject<HTMLElement | null>, dragging: boolean): void {
  const rects = useRef(new Map<string, { left: number; top: number }>())
  useLayoutEffect(() => {
    const el = root.current
    if (!el) return
    const reduce = typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches
    const next = new Map<string, { left: number; top: number }>()
    const chips = [...el.querySelectorAll<HTMLElement>('.unread-chip.pinned[data-bot-id]')]
    const moved = orderChanged([...rects.current.keys()], chips.map((c) => c.dataset.botId ?? ''))
    for (const chip of chips) {
      const id = chip.dataset.botId ?? ''
      const r = chip.getBoundingClientRect()
      next.set(id, { left: r.left, top: r.top })
      const prev = rects.current.get(id)
      if (dragging || reduce || !moved || !prev || typeof chip.animate !== 'function') continue
      const dx = prev.left - r.left
      const dy = prev.top - r.top
      if (Math.abs(dx) < 1 && Math.abs(dy) < 1) continue
      chip.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: 'translate(0, 0)' }], {
        duration: 220,
        easing: 'cubic-bezier(0.2, 0.8, 0.2, 1)',
      })
    }
    rects.current = next
  })
}
