import { useEffect } from 'react'
import type { KeyboardEvent, RefObject } from 'react'

const ITEMS = '[role="menuitem"],[role="menuitemradio"],[role="menuitemcheckbox"]'

function itemsOf(menu: HTMLElement): HTMLElement[] {
  return [...menu.querySelectorAll<HTMLElement>(ITEMS)].filter(
    (el) => !(el as HTMLButtonElement).disabled && el.getAttribute('aria-disabled') !== 'true',
  )
}

function focusStart(menu: HTMLElement) {
  const items = itemsOf(menu)
  const start = items.find((el) => el.getAttribute('aria-checked') === 'true') ?? items[0] ?? menu
  start.focus({ preventScroll: true })
}

export function useMenuKeys(
  open: boolean,
  menuRef: RefObject<HTMLElement | null>,
  triggerRef: RefObject<HTMLElement | null>,
  close: () => void,
  contentKey?: unknown,
) {
  useEffect(() => {
    if (!open) return
    const menu = menuRef.current
    const trigger = triggerRef.current
    if (!menu) return
    const raf = requestAnimationFrame(() => focusStart(menu))
    return () => {
      cancelAnimationFrame(raf)
      const a = document.activeElement
      if (!a || a === document.body || menu.contains(a)) trigger?.focus({ preventScroll: true })
    }
  }, [open, menuRef, triggerRef])

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
