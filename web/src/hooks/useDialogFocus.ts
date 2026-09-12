import { useEffect, useRef } from 'react'
import type { RefObject } from 'react'

const FOCUSABLE = [
  'a[href]',
  'area[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  'iframe',
  '[contenteditable="true"]',
  '[tabindex]',
].join(',')

function visible(el: HTMLElement) {
  return el.offsetParent !== null || el.getClientRects().length > 0
}

/** Return the tabbable descendants in the same order the browser uses for Tab. */
export function focusableIn(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => {
    if ((el.getAttribute('tabindex') ?? '').trim().startsWith('-')) return false
    if (el.hasAttribute('disabled') || el.hasAttribute('inert') || el.closest('[inert]')) return false
    if (el.closest('[aria-hidden="true"]')) return false
    const fieldset = el.closest('fieldset[disabled]')
    if (fieldset) {
      const legend = fieldset.querySelector(':scope > legend')
      if (!legend || !legend.contains(el)) return false
    }
    return visible(el)
  })
}

const activeDialogs: symbol[] = []

function inForeignModal(target: Node, root: HTMLElement) {
  const element = target instanceof Element ? target : target.parentElement
  const dialog = element?.closest<HTMLElement>('[aria-modal="true"], dialog[open]')
  return Boolean(dialog && dialog !== root && !root.contains(dialog))
}

export interface DialogFocusOptions {
  /** A preferred opening target; the first tabbable descendant is the fallback. */
  initialFocus?: () => HTMLElement | null | undefined
}

/**
 * Moves focus into an open dialog, traps Tab within it, and restores the opener on close.
 * A stack lets a confirm dialog safely sit on top of another dialog.
 */
export function useDialogFocus(
  open: boolean,
  rootRef: RefObject<HTMLElement | null>,
  { initialFocus }: DialogFocusOptions = {},
) {
  const initialFocusRef = useRef(initialFocus)
  useEffect(() => {
    initialFocusRef.current = initialFocus
  })

  useEffect(() => {
    if (!open) return

    const token = Symbol('dialog-focus')
    activeDialogs.push(token)
    const isTop = () => activeDialogs[activeDialogs.length - 1] === token
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    let openRoot = rootRef.current

    const focusInside = () => {
      const root = rootRef.current
      if (!root) return
      openRoot = root
      const preferred = initialFocusRef.current?.()
      const tabbable = focusableIn(root)
      const target = preferred && root.contains(preferred) && tabbable.includes(preferred) ? preferred : tabbable[0] ?? root
      if (target === root && !root.hasAttribute('tabindex')) root.setAttribute('tabindex', '-1')
      target.focus()
    }

    const raf = requestAnimationFrame(focusInside)
    const onKeyDown = (event: KeyboardEvent) => {
      if (!isTop() || event.key !== 'Tab') return
      const root = rootRef.current
      if (!root) return
      if (document.activeElement && inForeignModal(document.activeElement, root)) return
      const tabbable = focusableIn(root)
      if (tabbable.length === 0) {
        event.preventDefault()
        focusInside()
        return
      }
      const current = document.activeElement
      const first = tabbable[0]
      const last = tabbable[tabbable.length - 1]
      const outside = !root.contains(current)
      if (event.shiftKey && (outside || current === root || current === first)) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && (outside || current === root || current === last)) {
        event.preventDefault()
        first.focus()
      }
    }

    const onFocusIn = (event: FocusEvent) => {
      if (!isTop() || !(event.target instanceof Node)) return
      const root = rootRef.current
      if (!root || root.contains(event.target)) return
      if (inForeignModal(event.target, root)) return
      focusInside()
    }

    document.addEventListener('keydown', onKeyDown, true)
    document.addEventListener('focusin', onFocusIn, true)
    return () => {
      cancelAnimationFrame(raf)
      document.removeEventListener('keydown', onKeyDown, true)
      document.removeEventListener('focusin', onFocusIn, true)
      const index = activeDialogs.indexOf(token)
      if (index !== -1) activeDialogs.splice(index, 1)
      const current = document.activeElement
      const focusIsLeaving = current === null || current === document.body || (!!openRoot && openRoot.contains(current))
      if (opener?.isConnected && focusIsLeaving) opener.focus()
    }
  }, [open, rootRef])
}
