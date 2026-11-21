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
  initialFocus?: () => HTMLElement | null | undefined
}

export function useDialogFocus(
  open: boolean,
  rootRef: RefObject<HTMLElement | null>,
  { initialFocus }: DialogFocusOptions = {},
) {
  const initialFocusRef = useRef(initialFocus)
  useEffect(() => {
    initialFocusRef.current = initialFocus
  })

  const prevOpenRef = useRef(open)
  const renderOpenerRef = useRef<HTMLElement | null>(null)
  if (!prevOpenRef.current && open) {
    renderOpenerRef.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
  }
  prevOpenRef.current = open

  useEffect(() => {
    if (!open) return

    const token = Symbol('dialog-focus')
    activeDialogs.push(token)
    const isTop = () => activeDialogs[activeDialogs.length - 1] === token
    const capturedOpener = renderOpenerRef.current
    renderOpenerRef.current = null
    const fallbackOpener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const root = rootRef.current
    const opener = (capturedOpener && (!root || !root.contains(capturedOpener))) ? capturedOpener : fallbackOpener
    let openRoot = root

    const focusInside = () => {
      const currentRoot = rootRef.current
      if (!currentRoot) return
      openRoot = currentRoot
      const preferred = initialFocusRef.current?.()
      const tabbable = focusableIn(currentRoot)
      const target = preferred && currentRoot.contains(preferred) && tabbable.includes(preferred) ? preferred : tabbable[0] ?? currentRoot
      if (target === currentRoot && !currentRoot.hasAttribute('tabindex')) currentRoot.setAttribute('tabindex', '-1')
      target.focus()
    }

    const raf = requestAnimationFrame(focusInside)
    const onKeyDown = (event: KeyboardEvent) => {
      if (!isTop() || event.key !== 'Tab') return
      const currentRoot = rootRef.current
      if (!currentRoot) return
      if (document.activeElement && inForeignModal(document.activeElement, currentRoot)) return
      const tabbable = focusableIn(currentRoot)
      if (tabbable.length === 0) {
        event.preventDefault()
        focusInside()
        return
      }
      const current = document.activeElement
      const first = tabbable[0]
      const last = tabbable[tabbable.length - 1]
      const outside = !currentRoot.contains(current)
      if (event.shiftKey && (outside || current === currentRoot || current === first)) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && (outside || current === currentRoot || current === last)) {
        event.preventDefault()
        first.focus()
      }
    }

    const onFocusIn = (event: FocusEvent) => {
      if (!isTop() || !(event.target instanceof Node)) return
      const currentRoot = rootRef.current
      if (!currentRoot || currentRoot.contains(event.target)) return
      if (inForeignModal(event.target, currentRoot)) return
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
