import { useCallback, useEffect, useRef, type RefObject } from 'react'

export interface RefocusInput {
  wasSending: boolean
  sending: boolean
  /** 按下送出當下，焦點是不是在輸入框。 */
  hadFocus: boolean
  /** 現在焦點是不是掉到 body（textarea disabled 的副作用）；使用者自己移去別處就不是。 */
  focusLost: boolean
  phone: boolean
}

export function shouldRefocusAfterSend({ wasSending, sending, hadFocus, focusLost, phone }: RefocusInput): boolean {
  return wasSending && !sending && hadFocus && focusLost && !phone
}

/**
 * 送出期間輸入框是 disabled，焦點會掉到 body，送完沒人叫回來。
 * 回傳的 `noteSend` 要在 setSending(true) 之前呼叫（此時 textarea 還沒被 disable，才量得到焦點）。
 * 手機不還焦點（不彈鍵盤）。
 */
export function useRefocusAfterSend(sending: boolean, ref: RefObject<HTMLTextAreaElement | null>, phone: boolean) {
  const hadFocus = useRef(false)
  const wasSending = useRef(false)
  const noteSend = useCallback(() => {
    hadFocus.current = document.activeElement === ref.current
  }, [ref])
  useEffect(() => {
    const el = ref.current
    const active = document.activeElement
    const focusLost = !active || active === document.body
    if (el && shouldRefocusAfterSend({ wasSending: wasSending.current, sending, hadFocus: hadFocus.current, focusLost, phone })) {
      el.focus({ preventScroll: true })
    }
    wasSending.current = sending
  }, [sending, ref, phone])
  return noteSend
}
