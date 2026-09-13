import { useEffect, useRef, type RefObject } from 'react'

/**
 * 單行輸入框按 Enter 收下。Android 軟鍵盤送 keyCode 229、keydown 不成立（2026-09-09 使用者回報），
 * 改接 `beforeinput`。與 `useEnterToSend` 相反是刻意的：名字是單行的。
 */
export function useEnterCommit(ref: RefObject<HTMLInputElement | null>, commit: () => void) {
  const latest = useRef(commit)
  useEffect(() => {
    latest.current = commit
  })

  useEffect(() => {
    const el = ref.current
    if (!el) return
    const onBeforeInput = (ev: InputEvent) => {
      const linebreak =
        ev.inputType === 'insertLineBreak' ||
        ev.inputType === 'insertParagraph' ||
        (ev.inputType === 'insertText' && (ev.data === '\n' || ev.data === '\r' || ev.data === '\r\n'))
      if (!linebreak) return
      ev.preventDefault()
      latest.current()
    }
    el.addEventListener('beforeinput', onBeforeInput)
    return () => el.removeEventListener('beforeinput', onBeforeInput)
  }, [ref])

  return { enterKeyHint: 'done' as const }
}
