import { useEffect, useId, useRef } from 'react'
import type { ReactNode } from 'react'
import { focusableIn, useDialogFocus } from '../hooks/useDialogFocus'

/** Centred popup for the sidebar's big forms. The body keeps `sheet-body` so the existing form styling still applies. */
export function Modal({
  open,
  title,
  subtitle,
  width = 520,
  children,
  onClose,
}: {
  open: boolean
  title: string
  /** Small dimmed line after the title — e.g. which project a new bot lands in. */
  subtitle?: ReactNode
  width?: number
  children: ReactNode
  onClose: () => void
}) {
  const titleId = useId()
  const bodyRef = useRef<HTMLDivElement>(null)
  const dialogRef = useRef<HTMLDivElement>(null)

  // Land on the first field; `focusableIn` skips disabled controls, which would drop focus on the body.
  useDialogFocus(open, dialogRef, {
    initialFocus: () => (bodyRef.current ? focusableIn(bodyRef.current)[0] : null),
  })

  // Ref so an inline `onClose` doesn't re-register the Escape listener every render.
  const onCloseRef = useRef(onClose)
  useEffect(() => {
    onCloseRef.current = onClose
  })

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      // A picker or menu inside the body handles Escape first; only close when nothing did.
      if (e.defaultPrevented) return
      // Stacked modals: the outer listener runs first, so decide ownership by the focus-trapped target.
      const dialog = dialogRef.current
      if (dialog && e.target instanceof Node && !dialog.contains(e.target)) return
      e.preventDefault()
      onCloseRef.current()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open])

  if (!open) return null

  return (
    <div className="modal-backdrop" role="presentation" onMouseDown={onClose}>
      <div
        className="modal"
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        // A CSS var, not a fixed inline width: `.modal:has(.dirpicker)` needs to widen it.
        style={{ ['--modal-w' as string]: `${width}px` }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <strong id={titleId}>{title}</strong>
          {subtitle ? <span className="modal-sub">{subtitle}</span> : null}
          <button type="button" className="icon-btn" aria-label="關閉" title="關閉（Esc）" onClick={onClose}>
            ✕
          </button>
        </div>
        <div className="modal-body sheet-body" ref={bodyRef}>
          {children}
        </div>
      </div>
    </div>
  )
}
