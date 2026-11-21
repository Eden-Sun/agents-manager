import { useEffect, useRef, type RefObject } from 'react'

export function useEnterCommit(ref: RefObject<HTMLInputElement | null>, commit: () => void) {
  const latest = useRef(commit)
  useEffect(() => {
    latest.current = commit
  })

  useEffect(() => {
    const el = ref.current
    if (!el) return
    const onBeforeInput = (ev: InputEvent) => {
      if (ev.isComposing) return
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
  })

  return { enterKeyHint: 'done' as const }
}
