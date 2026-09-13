import { useEffect } from 'react'
import type { KeyboardEvent, RefObject } from 'react'

const ITEMS = '[role="menuitem"],[role="menuitemradio"],[role="menuitemcheckbox"]'

function itemsOf(menu: HTMLElement): HTMLElement[] {
  return [...menu.querySelectorAll<HTMLElement>(ITEMS)].filter(
    (el) => !(el as HTMLButtonElement).disabled && el.getAttribute('aria-disabled') !== 'true',
  )
}

/** 已勾選的那一項（menuitemradio），沒有就第一項；一項都點不了就停在選單本身。 */
function focusStart(menu: HTMLElement) {
  const items = itemsOf(menu)
  const start = items.find((el) => el.getAttribute('aria-checked') === 'true') ?? items[0] ?? menu
  start.focus({ preventScroll: true })
}

/**
 * `role="menu"` 彈窗鍵盤行為（WAI-ARIA APG menu button）：方向鍵／Home/End 移動，Esc／Tab 關掉並還焦點給觸發鍵。
 * 項目用 `tabIndex={-1}`。處理過的鍵要 `stopPropagation`：portal 的 React 事件仍會冒泡回觸發鍵那一列。
 */
export function useMenuKeys(
  open: boolean,
  menuRef: RefObject<HTMLElement | null>,
  triggerRef: RefObject<HTMLElement | null>,
  close: () => void,
  /** 選單內容的版本（例如載入完的清單）；變了而焦點掉出去時，重新把焦點放回選單。 */
  contentKey?: unknown,
) {
  useEffect(() => {
    if (!open) return
    const menu = menuRef.current
    const trigger = triggerRef.current
    if (!menu) return
    // 等一格：第一次 render 還是 `visibility: hidden`，focus() 會落空。
    const raf = requestAnimationFrame(() => focusStart(menu))
    return () => {
      cancelAnimationFrame(raf)
      const a = document.activeElement
      if (!a || a === document.body || menu.contains(a)) trigger?.focus({ preventScroll: true })
    }
  }, [open, menuRef, triggerRef])

  // 內容重建（如模型清單載入完）焦點掉回 body 時撿回來，否則方向鍵沒反應。
  useEffect(() => {
    const menu = menuRef.current
    if (!open || !menu) return
    const a = document.activeElement
    if (!a || a === document.body) focusStart(menu)
  }, [open, menuRef, contentKey])

  return (e: KeyboardEvent<HTMLElement>) => {
    const menu = menuRef.current
    if (!menu) return
    if (e.key === 'Escape') {
      e.preventDefault()
      e.stopPropagation()
      close()
      return
    }
    if (e.key === 'Tab') {
      // portal 在 body 尾端，放 Tab 會掉到頁尾。
      e.preventDefault()
      e.stopPropagation()
      close()
      return
    }
    const step = { ArrowDown: 1, ArrowRight: 1, ArrowUp: -1, ArrowLeft: -1 }[e.key]
    if (step === undefined && e.key !== 'Home' && e.key !== 'End') return
    const items = itemsOf(menu)
    e.preventDefault()
    e.stopPropagation()
    if (items.length === 0) return
    const at = items.indexOf(document.activeElement as HTMLElement)
    const next =
      e.key === 'Home'
        ? 0
        : e.key === 'End'
          ? items.length - 1
          : at < 0
            ? step! > 0
              ? 0
              : items.length - 1
            : (at + step! + items.length) % items.length
    items[next].focus()
  }
}
