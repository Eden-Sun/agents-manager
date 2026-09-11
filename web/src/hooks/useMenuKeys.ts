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
 * `role="menu"` 彈窗的鍵盤行為（WAI-ARIA APG 的 menu button）：
 * - 打開時焦點進選單：落在已勾選的那一項（menuitemradio），沒有就第一項；
 * - ↑/↓（chip 橫排，所以 ←/→ 也算）在項目間移動、Home/End 跳頭尾，Enter/Space 交給按鈕自己的 click；
 * - Esc 與 Tab 都關掉選單、焦點回觸發鍵。
 * 關掉時焦點若還留在選單裡（被移掉之後落回 body），就還給觸發鍵——不然鍵盤使用者選完一項就迷路。
 *
 * 項目用 `tabIndex={-1}`：整個選單只靠方向鍵走，Tab 不會一格一格停在裡面。
 * 處理過的鍵會 `stopPropagation`：選單常是 portal，React 事件照樣會冒泡回觸發鍵所在的
 * 那一列（例如側欄 bot 列的 ↑/↓ 換 bot）。
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
    // 等一格：浮動選單第一次 render 時還是 `visibility: hidden`（位置在 layout effect 裡才算好），
    // 那時候 focus() 會落空。
    const raf = requestAnimationFrame(() => focusStart(menu))
    return () => {
      cancelAnimationFrame(raf)
      const a = document.activeElement
      if (!a || a === document.body || menu.contains(a)) trigger?.focus({ preventScroll: true })
    }
  }, [open, menuRef, triggerRef])

  // 選單開著時內容換掉了（例如模型清單第一次打開才載入，整排按鈕重建），原本有焦點的那一項
  // 被移掉、焦點掉回 body——撿回來，不然方向鍵就沒反應了。
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
      // 選單多半是掛在 body 尾端的 portal，放 Tab 照 DOM 順序走會掉到頁尾；關掉、焦點回觸發鍵。
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
