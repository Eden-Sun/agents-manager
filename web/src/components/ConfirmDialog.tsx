import { useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { ReactNode } from 'react'
import { useDialogFocus } from '../hooks/useDialogFocus'

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
  /**
   * Confirm stays disabled regardless of the typed text — for a delete the daemon would
   * 409 anyway (project with an active run, host / identity still in use); the body says why.
   */
  confirmDisabled?: boolean
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
        // Window-level capture, so with a confirm stacked over another dialog the outer
        // listener runs first. Focus is trapped in the innermost one, so let the keystroke's
        // target decide which dialog the Escape belongs to.
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

  // Focus the require-text field when present; otherwise focus Cancel — never Confirm, so an
  // accidental Enter or Space on a just-opened destructive dialog must not go through.
  useDialogFocus(open, dialogRef, {
    initialFocus: () => (needsMatch ? inputRef.current : cancelRef.current),
  })

  if (!open) return null

  // 掛到 body：原本就地渲染時，會被外層的 stacking context（`.main`／聊天面板）壓在
  // `.shelf`（z-index 30）底下——手機上「圖片暫存」那條蓋住按鈕，點不到（2026-09-10）。
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
