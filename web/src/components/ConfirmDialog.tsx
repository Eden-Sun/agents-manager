import { useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { ReactNode } from 'react'
import { useDialogFocus } from '../hooks/useDialogFocus'
import './confirmDialog.css'

export interface ConfirmDialogProps {
  open: boolean
  title: string
  body: ReactNode
  confirmLabel: string
  cancelLabel?: string
  danger?: boolean
  /** When set, confirm stays disabled until the input equals this string. */
  requireText?: string
  requireTextLabel?: string
  /** Confirm stays disabled regardless of text — the daemon would 409 anyway; the body says why. */
  confirmDisabled?: boolean
  width?: number
  onConfirm: () => void
  onCancel: () => void
}

/** Reusable confirm modal. Escape / Cancel never call onConfirm; optional requireText gates confirm. */
export function ConfirmDialog({
  open,
  title,
  body,
  confirmLabel,
  cancelLabel = '取消',
  danger = false,
  requireText,
  requireTextLabel,
  confirmDisabled = false,
  width = 360,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const titleId = useId()
  const inputRef = useRef<HTMLInputElement>(null)
  const cancelRef = useRef<HTMLButtonElement>(null)
  const dialogRef = useRef<HTMLDivElement>(null)
  const [typed, setTyped] = useState('')
  const needsMatch = requireText !== undefined
  const matched = !needsMatch || typed === requireText

  // Reset during render, not in an effect: inline `onCancel` props re-ran the effect → "Maximum update depth exceeded".
  const [lastOpen, setLastOpen] = useState(open)
  if (lastOpen !== open) {
    setLastOpen(open)
    if (!open && typed !== '') setTyped('')
  }

  // Via ref so the Escape listener needn't depend on the unstable prop.
  const onCancelRef = useRef(onCancel)
  useEffect(() => {
    onCancelRef.current = onCancel
  })

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        // Window-level capture runs the outer dialog first; let the target decide which dialog owns Escape.
        const dialog = dialogRef.current
        if (dialog && e.target instanceof Node && !dialog.contains(e.target)) return
        e.preventDefault()
        e.stopPropagation()
        onCancelRef.current()
      }
    }
    window.addEventListener('keydown', onKey, true)
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open])

  // Focus the require-text field or Cancel — never Confirm, so a stray Enter can't go through.
  useDialogFocus(open, dialogRef, {
    initialFocus: () => (needsMatch ? inputRef.current : cancelRef.current),
  })

  if (!open) return null

  // 掛到 body：就地渲染會被外層 stacking context 壓在 `.shelf`（z-index 30）底下，手機點不到（2026-09-10）。
  return createPortal(
    <div className="confirm-backdrop" role="presentation" onMouseDown={onCancel}>
      <div
        className="confirm-dialog"
        ref={dialogRef}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        style={{ width }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <h2 id={titleId} className="confirm-title">
          {title}
        </h2>
        <div className="confirm-body">{body}</div>
        {needsMatch ? (
          <label className="confirm-require">
            <span>{requireTextLabel ?? `請輸入「${requireText}」以確認`}</span>
            <input
              ref={inputRef}
              type="text"
              value={typed}
              spellCheck={false}
              autoComplete="off"
              aria-label={requireTextLabel ?? `輸入 ${requireText}`}
              onChange={(e) => setTyped(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && matched && !confirmDisabled) {
                  e.preventDefault()
                  onConfirm()
                }
              }}
            />
          </label>
        ) : null}
        <div className="confirm-actions">
          <button type="button" className="btn" ref={cancelRef} onClick={onCancel}>
            {cancelLabel}
          </button>
          <button
            type="button"
            className={`btn${danger ? ' danger' : ' primary'}`}
            disabled={!matched || confirmDisabled}
            onClick={onConfirm}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  )
}
