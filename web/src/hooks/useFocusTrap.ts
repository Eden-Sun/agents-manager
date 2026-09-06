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

function isVisible(el: HTMLElement) {
  // `offsetParent` is null for `display:none` (and for anything inside it); position:fixed
  // elements report null too, so fall back to the box size before calling it hidden.
  return el.offsetParent !== null || el.getClientRects().length > 0
}

/**
 * A control inside `<fieldset disabled>` is disabled without carrying the attribute. The
 * exception is the first legend's contents, which stay enabled.
 */
function inDisabledFieldset(el: HTMLElement) {
  const fieldset = el.closest('fieldset[disabled]')
  if (!fieldset) return false
  const legend = fieldset.querySelector(':scope > legend')
  return !(legend && legend.contains(el))
}

/** Tabbable descendants of `root`, in DOM order — the order Tab itself walks. */
export function focusableIn(root: HTMLElement): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter((el) => {
    // `tabindex="-1"` is programmatically focusable but not tabbable, and it can sit on a
    // button or input too — so this is checked on every candidate, not just via the selector.
    if ((el.getAttribute('tabindex') ?? '').trim().startsWith('-')) return false
    if (el.hasAttribute('inert') || el.closest('[inert]')) return false
    if (el.closest('[aria-hidden="true"]')) return false
    if (inDisabledFieldset(el)) return false
    return isVisible(el)
  })
}

/**
 * True when focus landed in some other modal dialog stacked over this one — BlockedModal is a
 * portal that manages its own keyboard and never registers here, and pulling focus back out of
 * it would fight whatever it is doing. Deliberately matches any `aria-modal` dialog, not just
 * the ones this hook drives.
 */
function inForeignModal(target: Node, root: HTMLElement) {
  const el = target instanceof Element ? target : target.parentElement
  const dialog = el?.closest<HTMLElement>('[aria-modal="true"], dialog[open]')
  return !!dialog && dialog !== root && !root.contains(dialog)
}

/**
 * The layers currently trapping focus, innermost last. A modal opened from inside another
 * modal (delete-bot's confirm over the settings popup) must not have the outer trap yank
 * focus back out of it, so only the top of this stack enforces anything.
 */
const stack: symbol[] = []

export interface FocusTrapOptions {
  /**
   * Element to focus when the layer opens. Returning null falls back to the first tabbable
   * child, and then to the container itself so the keyboard never lands behind the backdrop.
   */
  initialFocus?: () => HTMLElement | null | undefined
}

/**
 * Keeps keyboard focus inside `ref` while `active`, and puts it back where it came from on
 * close. Safe to nest: each layer registers on a shared stack and defers to the innermost.
 */
export function useFocusTrap(
  active: boolean,
  ref: RefObject<HTMLElement | null>,
  { initialFocus }: FocusTrapOptions = {},
) {
  // Read the callback through a ref: call sites pass an inline arrow, and making it a
  // dependency would re-run the whole effect (re-stealing focus) on every render.
  const initialFocusRef = useRef(initialFocus)
  useEffect(() => {
    initialFocusRef.current = initialFocus
  })

  useEffect(() => {
    if (!active) return
    const id = Symbol('focus-trap')
    stack.push(id)
    const isTop = () => stack[stack.length - 1] === id

    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null
    // Kept up to date by every run below; read again in the cleanup, where the ref is gone.
    let openRoot = ref.current

    const focusFirst = () => {
      const root = ref.current
      if (!root) return
      openRoot = root
      const wanted = initialFocusRef.current?.()
      // A caller can hand back a control that is disabled by the form's current state;
      // focusing it is a silent no-op, so fall through to the first real one.
      const usable = wanted && root.contains(wanted) && focusableIn(root).includes(wanted) ? wanted : null
      const target = usable ?? focusableIn(root)[0] ?? root
      if (target === root && !root.hasAttribute('tabindex')) root.setAttribute('tabindex', '-1')
      target.focus()
    }
    // A frame late: the dialog's children (and any autofocus inside them) exist by then.
    const raf = requestAnimationFrame(focusFirst)

    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== 'Tab' || !isTop()) return
      const root = ref.current
      if (!root) return
      // An unmanaged dialog stacked on top (BlockedModal) handles its own keyboard.
      if (document.activeElement && inForeignModal(document.activeElement, root)) return
      const items = focusableIn(root)
      if (items.length === 0) {
        // Nothing to tab to — keep focus on the dialog rather than letting it escape.
        e.preventDefault()
        return
      }
      const first = items[0]
      const last = items[items.length - 1]
      const current = document.activeElement
      // Focus parked on the container itself (nothing was focusable when it opened, or the
      // trap pulled it back there) is not in `items`: Tab from there goes to the first
      // control and Shift+Tab to the last, instead of escaping the dialog entirely.
      const onRoot = current === root
      const outside = !root.contains(current)
      if (e.shiftKey && (onRoot || outside || current === first)) {
        e.preventDefault()
        last.focus()
      } else if (!e.shiftKey && (onRoot || outside || current === last)) {
        e.preventDefault()
        first.focus()
      }
    }

    // Tab is not the only way out (a click on the page behind, a browser find bar). Pull
    // focus back whenever it lands outside the innermost layer.
    const onFocusIn = (e: FocusEvent) => {
      if (!isTop()) return
      const root = ref.current
      if (!root) return
      if (!(e.target instanceof Node)) return
      if (root.contains(e.target)) return
      if (inForeignModal(e.target, root)) return
      focusFirst()
    }

    document.addEventListener('keydown', onKeyDown, true)
    document.addEventListener('focusin', onFocusIn, true)

    return () => {
      cancelAnimationFrame(raf)
      document.removeEventListener('keydown', onKeyDown, true)
      document.removeEventListener('focusin', onFocusIn, true)
      const at = stack.indexOf(id)
      if (at !== -1) stack.splice(at, 1)
      // Restore to whatever opened this layer — including an opener that lives inside a
      // still-open outer dialog. Skip it when focus has already moved somewhere real (a
      // newly opened layer took it), which is anywhere other than the detached dialog or
      // the body the browser falls back to.
      const now = document.activeElement
      // `ref.current` is already detached by the time an unmount cleanup runs, so the node
      // captured while the layer was open is the one that can answer "is focus still in it".
      const closing = openRoot
      const drifted = now === null || now === document.body || (!!closing && closing.contains(now))
      if (previous && previous.isConnected && drifted) previous.focus()
    }
  }, [active, ref])
}
