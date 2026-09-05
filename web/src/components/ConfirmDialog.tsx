import { useEffect, useId, useRef, useState } from 'react'
import type { ReactNode } from 'react'

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
  width?: number
  onConfirm: () => void
  onCancel: () => void
}

/**
 * Reusable confirm modal. Escape / Cancel never call onConfirm.
 * Optional requireText match gates the confirm button (delete Bot).
 */
export function ConfirmDialog({
  open,
  title,
  body,
  confirmLabel,
  cancelLabel = '取消',
  danger = false,
  requireText,
  requireTextLabel,
  width = 360,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const titleId = useId()
  const inputRef = useRef<HTMLInputElement>(null)
  const [typed, setTyped] = useState('')
  const needsMatch = requireText !== undefined
  const matched = !needsMatch || typed === requireText

  useEffect(() => {
    if (!open) {
      setTyped('')
      return
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault()
        e.stopPropagation()
        onCancel()
      }
    }
    window.addEventListener('keydown', onKey, true)
    // Focus the require-text field, otherwise the cancel button for Escape-friendly keyboard use.
    requestAnimationFrame(() => {
      if (needsMatch) inputRef.current?.focus()
    })
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open, onCancel, needsMatch])

  if (!open) return null

  return (
    <div className="confirm-backdrop" role="presentation" onMouseDown={onCancel}>
      <div
        className="confirm-dialog"
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
                if (e.key === 'Enter' && matched) {
                  e.preventDefault()
                  onConfirm()
                }
              }}
            />
          </label>
        ) : null}
        <div className="confirm-actions">
          <button type="button" className="btn" onClick={onCancel}>
            {cancelLabel}
          </button>
          <button
            type="button"
            className={`btn${danger ? ' danger' : ' primary'}`}
            disabled={!matched}
            onClick={onConfirm}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  )
}
