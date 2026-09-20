/**
 * 主力那列（★）的拖曳與鍵盤重排（issue #344）。用 pointer events 而不是 HTML5 DnD：後者在觸控上不會動。
 * - 滑鼠：按住移動超過 5px 開始拖。
 * - 觸控：長按 350ms 才開始拖（之前手指移動超過 10px 視為捲動，取消）——手機那排本來就是橫捲。
 * - 鍵盤：聚焦晶片後 Ctrl+←／→ 移一格，並用 live region 報位置。
 * 順序計算在 `lib/pinnedOrder.ts`（純函式、有測試）。
 */
import { useCallback, useEffect, useRef, useState } from 'react'
import { flushSync } from 'react-dom'
import type { KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react'
import { moveBefore, pickSlot, type Box } from '../lib/pinnedOrder'

const MOUSE_SLOP = 5
const TOUCH_SLOP = 10
const LONG_PRESS_MS = 350

export interface PinnedDnd {
  dragId: string | null
  /** 目前放開會插在誰前面（null＝這組最後）；沒在拖＝undefined。 */
  before: string | null | undefined
  /** 畫面上最後一顆主力晶片（放到最後面時在它右邊畫落點線）。 */
  lastId: string | null
  /** 落點讓位：這些晶片往右推 `gap` px（同一行、落點之後的）。 */
  shifted: string[]
  gap: number
  /** 落點在行尾時，那一行最後一顆（標示畫在它右邊）。 */
  after: string | null
  /** 拖曳中的晶片跟著游標的位移（px，相對它原本的位置）；沒在拖＝0,0。 */
  offset: { x: number; y: number }
  /** 給 live region 的最新一句。 */
  announce: string
  onPointerDown: (e: ReactPointerEvent<HTMLElement>, id: string) => void
  onKeyDown: (e: ReactKeyboardEvent<HTMLElement>, id: string) => void
  /** 剛拖完的那一下 click 要吞掉（否則放開會順便選到那顆 bot）。 */
  consumeClick: () => boolean
}

/**
 * @param fullOrder 所有主力 bot 的完整順序（含側欄收起來的），存回 daemon 用這份
 * @param visibleOrder 畫面上看得到的主力晶片順序，鍵盤移動與落點用這份
 */
export function usePinnedDrag(
  fullOrder: string[],
  visibleOrder: string[],
  names: Record<string, string>,
  commit: (order: string[]) => void,
): PinnedDnd {
  const [drag, setDrag] = useState<{ id: string; before: string | null; ready: boolean; dx: number; dy: number; shift: string[]; after: string | null; gap: number } | null>(null)
  // 上一個鎖定的落點（遲滯）。
  const held = useRef<{ before: string | null | undefined }>({ before: undefined })
  const [announce, setAnnounce] = useState('')
  const suppress = useRef(false)
  // 事件處理在 window 上，讀最新的順序要靠 ref。
  const latest = useRef({ fullOrder, commit })
  useEffect(() => {
    latest.current = { fullOrder, commit }
  })
  const cleanup = useRef<(() => void) | null>(null)
  useEffect(() => () => cleanup.current?.(), [])

  const boxesOf = (from: HTMLElement): Box[] => {
    const scope = from.closest('.unread-group, .unread-bar') ?? document
    return [...scope.querySelectorAll<HTMLElement>('.unread-chip.pinned[data-bot-id]')].map((c) => {
      const r = c.getBoundingClientRect()
      // 量的是「沒被讓位推過」的位置：扣回它**目前實際**的 translateX（過渡到一半也對；減少動態時沒有位移就不扣）。
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
    let dragging = false
    let timer: ReturnType<typeof setTimeout> | null = null

    const stopTouchScroll = (ev: TouchEvent) => ev.preventDefault()
    const begin = () => {
      dragging = true
      suppress.current = true
      held.current = { before: undefined }
      setDrag({ id, before: null, ready: false, dx: 0, dy: 0, shift: [], after: null, gap: 0 })
      window.addEventListener('touchmove', stopTouchScroll, { passive: false })
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
      const dx = ev.clientX - start.x
      const dy = ev.clientY - start.y
      if (!dragging) {
        const far = Math.hypot(dx, dy)
        if (touch) {
          // 長按之前就移動＝使用者在捲動，放棄這次。
          if (far > TOUCH_SLOP) end()
          return
        }
        if (far < MOUSE_SLOP) return
        begin()
      }
      const slot = pickSlot(boxesOf(chip), ev.clientX, ev.clientY, id, held.current.before)
      const gap = Math.round(chip.getBoundingClientRect().width) + 8
      held.current = { before: slot.before }
      setDrag({ id, before: slot.before, ready: true, dx, dy, shift: slot.shift, after: slot.after, gap })
    }
    const up = (ev: PointerEvent) => {
      const was = dragging
      end()
      if (!was) return
      const before = pickSlot(boxesOf(chip), ev.clientX, ev.clientY, id, held.current.before).before
      const next = moveBefore(latest.current.fullOrder, id, before)
      // 同一次渲染收起拖曳狀態與套用新順序：分兩次渲染的話，中間那一格會讓晶片先「彈回原位」再滑去新位置（useChipFlip 量的是每次渲染後的位置）。
      flushSync(() => {
        setDrag(null)
        if (next) latest.current.commit(next)
      })
      // click 事件在 pointerup 之後同一個 task 內觸發；下一個 task 再放行。
      setTimeout(() => {
        suppress.current = false
      }, 0)
    }
    const cancel = () => {
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
        setAnnounce(`${names[id] ?? ''} 已經在最${dir < 0 ? '前' : '後'}面`)
        return
      }
      // 往左＝插在左邊那顆前面；往右＝插在右邊那顆的下一顆前面（沒有＝最後）。
      const before = dir < 0 ? visibleOrder[to] : (visibleOrder[to + 1] ?? null)
      const next = moveBefore(fullOrder, id, before)
      if (!next) return
      commit(next)
      setAnnounce(`${names[id] ?? ''} 移到主力第 ${to + 1} 位，共 ${visibleOrder.length} 位`)
      // 列重排後 DOM 節點換位，焦點可能掉；下一幀補回。
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
