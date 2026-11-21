import { useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { ReactNode } from 'react'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { isImeEnter } from '../lib/ime'
import './confirmDialog.css'
import { useT } from '../lib/i18n'

export interface ConfirmDialogProps {
  open: boolean
  title: string
  body: ReactNode
  confirmLabel: string
  cancelLabel?: string
  danger?: boolean
  requireText?: string
  requireTextLabel?: string
  confirmDisabled?: boolean
  secondaryLabel?: string
  secondaryDisabled?: boolean
  onSecondary?: () => void
  width?: number
  onConfirm: () => void
  onCancel: () => void
}

const FIRE_GUARD_MS = 800

export function ConfirmDialog({
  open,
  title,
  body,
  confirmLabel,
  cancelLabel,
  danger = false,
  requireText,
  requireTextLabel,
  confirmDisabled = false,
  secondaryLabel,
  secondaryDisabled = false,
  onSecondary,
  width = 360,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const t = useT()
  const titleId = useId()
  const bodyId = useId()
  const inputRef = useRef<HTMLInputElement>(null)
  const cancelRef = useRef<HTMLButtonElement>(null)
  const dialogRef = useRef<HTMLDivElement>(null)
  const [typed, setTyped] = useState('')
  const needsMatch = requireText !== undefined
  const matched = !needsMatch || typed === requireText

  const [lastOpen, setLastOpen] = useState(open)
  if (lastOpen !== open) {
    setLastOpen(open)
    if (!open && typed !== '') setTyped('')
  }

  const lastFire = useRef(0)
  const once = (fn: () => void) => () => {
    const now = Date.now()
    if (now - lastFire.current < FIRE_GUARD_MS) return
    lastFire.current = now
    fn()
  }

  const onCancelRef = useRef(onCancel)
  useEffect(() => {
    onCancelRef.current = onCancel
  })

  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        if (isImeEnter(e)) return
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

  useDialogFocus(open, dialogRef, {
    initialFocus: () => (needsMatch ? inputRef.current : cancelRef.current),
  })

  if (!open) return null

  return createPortal(
    <div className="confirm-backdrop" role="presentation" onMouseDown={onCancel} onClick={(e) => e.stopPropagation()}>
      <div
        className="confirm-dialog"
        ref={dialogRef}
        tabIndex={-1}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={bodyId}
        style={{ width }}
        onMouseDown={(e) => e.stopPropagation()}
      >
        <h2 id={titleId} className="confirm-title">
          {title}
        </h2>
        <div id={bodyId} className="confirm-body">{body}</div>
        {needsMatch ? (
          <label className="confirm-require">
            <span>{requireTextLabel ?? t('請輸入「{text}」以確認', { text: requireText ?? '' })}</span>
            <input
              ref={inputRef}
              type="text"
              value={typed}
              spellCheck={false}
              autoComplete="off"
              aria-label={requireTextLabel ?? t('輸入 {text}', { text: requireText ?? '' })}
              onChange={(e) => setTyped(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && matched && !confirmDisabled && !isImeEnter(e.nativeEvent)) {
                  e.preventDefault()
                  once(onConfirm)()
                }
              }}
            />
          </label>
        ) : null}
        <div className="confirm-actions">
          <button type="button" className="btn" ref={cancelRef} onClick={onCancel}>
            {cancelLabel ?? t('取消')}
          </button>
          {secondaryLabel && onSecondary ? (
            <button type="button" className="btn" disabled={secondaryDisabled} onClick={once(onSecondary)}>
              {secondaryLabel}
            </button>
          ) : null}
          <button
            type="button"
            className={`btn${danger ? ' danger' : ' primary'}`}
            disabled={!matched || confirmDisabled}
            onClick={once(onConfirm)}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  )
}
