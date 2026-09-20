import { useLayoutEffect, useRef } from 'react'
import type { RefObject } from 'react'

/**
 * 主力晶片換位的過渡（FLIP）：每次渲染後量每顆主力晶片的位置，跟上一次比，位移了就用 Web Animations 從舊位置滑到新位置。
 * - 放開晶片時它從「放開的地方」滑進新格（拖曳中它的位置含游標位移，上一次量到的就是那裡）、其他晶片從舊格滑開。
 * - 拖曳中不播（旁邊的晶片讓位由 CSS 的 margin 過渡處理，兩邊疊在一起會抖），只記位置。
 * - `prefers-reduced-motion: reduce` 不播：瞬間換位。
 */
export function useChipFlip(root: RefObject<HTMLElement | null>, dragging: boolean): void {
  const rects = useRef(new Map<string, { left: number; top: number }>())
  useLayoutEffect(() => {
    const el = root.current
    if (!el) return
    const reduce = typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches
    const next = new Map<string, { left: number; top: number }>()
    for (const chip of el.querySelectorAll<HTMLElement>('.unread-chip.pinned[data-bot-id]')) {
      const id = chip.dataset.botId ?? ''
      const r = chip.getBoundingClientRect()
      next.set(id, { left: r.left, top: r.top })
      const prev = rects.current.get(id)
      if (dragging || reduce || !prev || typeof chip.animate !== 'function') continue
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
