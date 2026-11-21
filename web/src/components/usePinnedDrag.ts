import { useCallback, useEffect, useRef, useState } from 'react'
import { flushSync } from 'react-dom'
import type { KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react'
import { endBefore, moveBefore, pickSlot, type Box } from '../lib/pinnedOrder'
import { t, useT } from '../lib/i18n'

const MOUSE_SLOP = 5
const TOUCH_SLOP = 10
const LONG_PRESS_MS = 350

export interface PinnedDnd {
  dragId: string | null
  before: string | null | undefined
  lastId: string | null
  shifted: string[]
  gap: number
  after: string | null
  offset: { x: number; y: number }
  announce: string
  onPointerDown: (e: ReactPointerEvent<HTMLElement>, id: string) => void
  onKeyDown: (e: ReactKeyboardEvent<HTMLElement>, id: string) => void
  consumeClick: () => boolean
}

export function usePinnedDrag(
  fullOrder: string[],
  visibleOrder: string[],
  names: Record<string, string>,
  commit: (order: string[]) => void,
): PinnedDnd {
  useT()
  const [drag, setDrag] = useState<{ id: string; before: string | null; ready: boolean; dx: number; dy: number; shift: string[]; after: string | null; gap: number } | null>(null)
  const held = useRef<{ before: string | null | undefined }>({ before: undefined })
  const [announce, setAnnounce] = useState('')
  const suppress = useRef(false)
  const latest = useRef({ fullOrder, visibleOrder, commit })
  useEffect(() => {
    latest.current = { fullOrder, visibleOrder, commit }
  })
  const cleanup = useRef<(() => void) | null>(null)
  useEffect(() => () => cleanup.current?.(), [])

  const boxesOf = (from: HTMLElement): Box[] => {
    const scope = from.closest('.unread-group, .unread-bar, .unread-pin-grid') ?? document
    return [...scope.querySelectorAll<HTMLElement>('.unread-chip.pinned[data-bot-id]')].map((c) => {
      const r = c.getBoundingClientRect()
      const t = getComputedStyle(c).transform
      const back = t && t !== 'none' ? new DOMMatrixReadOnly(t).m41 : 0
      return { id: c.dataset.botId ?? '', left: r.left - back, top: r.top, right: r.right - back, bottom: r.bottom }
    })
  }

  const onPointerDown = useCallback((e: ReactPointerEvent<HTMLElement>, id: string) => {
    if (e.pointerType === 'mouse' && e.button !== 0) return
    const chip = e.currentTarget
    const touch = e.pointerType !== 'mouse'
    const start = { x: e.clientX, y: e.clientY }
    const pid = e.pointerId
    let dragging = false
    let moved = false
    let timer: ReturnType<typeof setTimeout> | null = null

    const stopTouchScroll = (ev: TouchEvent) => ev.preventDefault()
    const begin = () => {
      dragging = true
      suppress.current = true
      held.current = { before: undefined }
      setDrag({ id, before: null, ready: false, dx: 0, dy: 0, shift: [], after: null, gap: 0 })
      window.addEventListener('touchmove', stopTouchScroll, { passive: false })
      if (touch) navigator.vibrate?.(10)
    }
    const end = () => {
      if (timer) clearTimeout(timer)
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      window.removeEventListener('pointercancel', cancel)
      window.removeEventListener('touchmove', stopTouchScroll)
      cleanup.current = null
    }
    const move = (ev: PointerEvent) => {
      if (ev.pointerId !== pid) return
      const dx = ev.clientX - start.x
      const dy = ev.clientY - start.y
      if (!dragging) {
        const far = Math.hypot(dx, dy)
        if (touch) {
          if (far > TOUCH_SLOP) end()
          return
        }
        if (far < MOUSE_SLOP) return
        begin()
      }
      if (touch && !moved && Math.hypot(dx, dy) <= TOUCH_SLOP) return
      moved = true
      const slot = pickSlot(boxesOf(chip), ev.clientX, ev.clientY, id, held.current.before)
      const gap = Math.round(chip.getBoundingClientRect().width) + 8
      held.current = { before: slot.before }
      setDrag({ id, before: slot.before, ready: true, dx, dy, shift: slot.shift, after: slot.after, gap })
    }
    const up = (ev: PointerEvent) => {
      if (ev.pointerId !== pid) return
      const was = dragging
      end()
      if (!was) return
      if (touch && !moved) {
        setDrag(null)
        setTimeout(() => {
          suppress.current = false
        }, 0)
        return
      }
      const picked = pickSlot(boxesOf(chip), ev.clientX, ev.clientY, id, held.current.before).before
      const { fullOrder: full, visibleOrder: shown } = latest.current
      const before = picked === null ? endBefore(full.filter((x) => x !== id), shown.filter((x) => x !== id)) : picked
      const next = moveBefore(full, id, before)
      flushSync(() => {
        setDrag(null)
        if (next) latest.current.commit(next)
      })
      setTimeout(() => {
        suppress.current = false
      }, 0)
    }
    const cancel = (ev: PointerEvent) => {
      if (ev.pointerId !== pid) return
      const was = dragging
      end()
      if (was) {
        setDrag(null)
        suppress.current = false
      }
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
    window.addEventListener('pointercancel', cancel)
    cleanup.current = end
    if (touch) timer = setTimeout(begin, LONG_PRESS_MS)
  }, [])

  const onKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLElement>, id: string) => {
      if (!e.ctrlKey || (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight')) return
      e.preventDefault()
      const at = visibleOrder.indexOf(id)
      if (at < 0) return
      const dir = e.key === 'ArrowLeft' ? -1 : 1
      const to = at + dir
      if (to < 0 || to >= visibleOrder.length) {
        setAnnounce(dir < 0 ? t('{name} 已經在最前面', { name: names[id] ?? '' }) : t('{name} 已經在最後面', { name: names[id] ?? '' }))
        return
      }
      const before = dir < 0 ? visibleOrder[to] : (visibleOrder[to + 1] ?? endBefore(fullOrder, visibleOrder))
      const next = moveBefore(fullOrder, id, before)
      if (!next) return
      commit(next)
      setAnnounce(t('{name} 移到主力第 {position} 位，共 {total} 位', { name: names[id] ?? '', position: to + 1, total: visibleOrder.length }))
      const chip = e.currentTarget
      requestAnimationFrame(() => {
        const again = chip.isConnected ? chip : document.querySelector<HTMLElement>(`.unread-chip[data-bot-id="${CSS.escape(id)}"]`)
        again?.focus()
      })
    },
    [commit, fullOrder, names, visibleOrder],
  )

  const consumeClick = useCallback(() => suppress.current, [])

  return { dragId: drag?.id ?? null, before: drag?.ready ? drag.before : undefined, offset: { x: drag?.dx ?? 0, y: drag?.dy ?? 0 }, shifted: drag?.ready ? drag.shift : [], gap: drag?.gap ?? 0, after: drag?.ready ? drag.after : null,
    lastId: visibleOrder[visibleOrder.length - 1] ?? null, announce, onPointerDown, onKeyDown, consumeClick }
}
