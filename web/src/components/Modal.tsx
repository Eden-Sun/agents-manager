import { useEffect, useId, useRef } from 'react'
import type { ReactNode } from 'react'
import { focusableIn, useDialogFocus } from '../hooks/useDialogFocus'
import { isImeEnter } from '../lib/ime'
import { useT } from '../lib/i18n'

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
  subtitle?: ReactNode
  width?: number
  children: ReactNode
  onClose: () => void
}) {
  const t = useT()
  const titleId = useId()
  const bodyRef = useRef<HTMLDivElement>(null)
  const dialogRef = useRef<HTMLDivElement>(null)

  useDialogFocus(open, dialogRef, {
    initialFocus: () => (bodyRef.current ? focusableIn(bodyRef.current)[0] : null),
  })

  const onCloseRef = useRef(onClose)
  useEffect(() => {
    onCloseRef.current = onClose
  })

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return
      if (isImeEnter(e)) return
      if (e.defaultPrevented) return
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
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        style={{ ['--modal-w' as string]: `${width}px` }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="modal-head">
          <strong id={titleId}>{title}</strong>
          {subtitle ? <span className="modal-sub">{subtitle}</span> : null}
          <button type="button" className="icon-btn" aria-label={t('關閉')} title={t('關閉（Esc）')} onClick={onClose}>
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
