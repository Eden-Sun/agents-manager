import { useEffect, useId, useRef } from 'react'
import type { ReactNode } from 'react'

/**
 * Centred popup for the sidebar's three big forms (新增 Project / 新增 Bot / 環境設定).
 * They used to replace the whole sidebar or push the footer around; as popups the bot
 * list stays visible behind them and every one of them closes the same way.
 *
 * The body keeps the `sheet-body` class so the form styling written for the old
 * in-sidebar sheets (field heights, the DirPicker's fill-the-height rule) still applies.
 */
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

  // Read through a ref so the Escape listener always sees the current handler without
  // making an inline `onClose={() => …}` prop re-register it on every render.
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
      e.preventDefault()
      onCloseRef.current()
    }
    window.addEventListener('keydown', onKey)
    // Land on the first field so the form is usable straight from the keyboard.
    const raf = requestAnimationFrame(() => {
      bodyRef.current?.querySelector<HTMLElement>('input, select, textarea, button')?.focus()
    })
    return () => {
      window.removeEventListener('keydown', onKey)
      cancelAnimationFrame(raf)
    }
  }, [open])

  if (!open) return null

  return (
    <div className="modal-backdrop" role="presentation" onMouseDown={onClose}>
      <div
        className="modal"
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
