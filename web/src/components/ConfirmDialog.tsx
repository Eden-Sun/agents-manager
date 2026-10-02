import { useEffect, useId, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { ReactNode } from 'react'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { isImeEnter } from '../lib/ime'
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
  /** 取消與確認之間的第二個選擇（例如「全新對話」vs「接續對話」）。 */
  secondaryLabel?: string
  secondaryDisabled?: boolean
  onSecondary?: () => void
  width?: number
  onConfirm: () => void
  onCancel: () => void
}

/** 連點防護的時間窗。 */
const FIRE_GUARD_MS = 800

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
  secondaryLabel,
  secondaryDisabled = false,
  onSecondary,
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

  // 確認／第二選項共用一道門：連點（或手快按了兩顆）只算一次。呼叫端不一定會同步關掉這個框（要等 API 回、或失敗要留著重試），
  // 沒有這道門，兩次 click 就是兩次刪除請求。時間窗不是永久鎖：失敗後使用者隔一下再按照樣能重試。
  const lastFire = useRef(0)
  const once = (fn: () => void) => () => {
    const now = Date.now()
    if (now - lastFire.current < FIRE_GUARD_MS) return
    lastFire.current = now
    fn()
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
    // React 事件沿 React 樹冒泡，不是沿 DOM：portal 出去的框裡的點擊會冒泡回渲染它的元件（額度卡片列、bot 列的 onClick
    // 於是被誤觸，#693／#694）。對話框是自成一層的，點擊不該離開它——跟 `onMouseDown` 擋 popover 的外點關閉同一個道理。
    <div className="confirm-backdrop" role="presentation" onMouseDown={onCancel} onClick={(e) => e.stopPropagation()}>
      <div
        className="confirm-dialog"
        ref={dialogRef}
        tabIndex={-1}
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
                // 輸入法選字確認的 Enter（isComposing／keyCode 229）不算：用中文輸入法打完名字，選字那一下就會把東西刪掉。
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
            {cancelLabel}
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
