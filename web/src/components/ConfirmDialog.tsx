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

  // Clearing the field belongs to the open/close transition, not to an effect: resetting it
  // from an effect re-rendered the component, and since every call site passes an inline
  // `onCancel={() => …}` (a new identity each render) the effect re-ran and reset again —
  // "Maximum update depth exceeded". Comparing against the previous prop during render is
  // React's own answer for this; it settles in one extra render because `lastOpen` then matches.
  const [lastOpen, setLastOpen] = useState(open)
  if (lastOpen !== open) {
    setLastOpen(open)
    if (!open && typed !== '') setTyped('')
  }

  // Read through a ref so the Escape listener always calls the current handler without making
  // the unstable prop a dependency of the effect below.
  const onCancelRef = useRef(onCancel)
  useEffect(() => {
    onCancelRef.current = onCancel
  })

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault()
        e.stopPropagation()
        onCancelRef.current()
      }
    }
    window.addEventListener('keydown', onKey, true)
    // Focus the require-text field, otherwise the cancel button for Escape-friendly keyboard use.
    requestAnimationFrame(() => {
      if (needsMatch) inputRef.current?.focus()
    })
    return () => window.removeEventListener('keydown', onKey, true)
  }, [open, needsMatch])

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
