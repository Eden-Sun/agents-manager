import { useCallback, useEffect, useRef, type RefObject } from 'react'

export interface RefocusInput {
  wasSending: boolean
  sending: boolean
  hadFocus: boolean
  focusLost: boolean
  phone: boolean
}

export function shouldRefocusAfterSend({ wasSending, sending, hadFocus, focusLost, phone }: RefocusInput): boolean {
  return wasSending && !sending && hadFocus && focusLost && !phone
}

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
