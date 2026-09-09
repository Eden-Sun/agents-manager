import { useEffect, useRef, type RefObject } from 'react'

/**
 * 單行輸入框（改名欄）的「按 Enter 就收下」。
 *
 * 桌面靠各自的 `onKeyDown`；手機不行——Android 的軟鍵盤送的是 keyCode 229，`key` 不是
 * `Enter`，所以 keydown 那條永遠不成立（2026-09-09 使用者回報：手機改名按 Enter 沒反應）。
 * 原生的 `beforeinput` 收得到 `insertLineBreak` / `insertParagraph`，有些 IME 則是把換行
 * 當文字送（`insertText` 且 `data` 整個就是換行），兩條都接。
 *
 * 這跟 composer 的 `useEnterToSend` 是相反的決定，而且是刻意的：訊息是多行的，名字不是。
 * 單行輸入框裡的「換行」除了送出沒有別的意思。
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
