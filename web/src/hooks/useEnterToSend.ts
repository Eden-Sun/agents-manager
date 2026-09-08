import { useEffect, useRef, type RefObject } from 'react'
import { PHONE_QUERY, useMediaQuery } from './useMediaQuery'

/**
 * 手機軟鍵盤的 Enter 不會送出（2026-09-08 使用者回報）。Android Chrome / Gboard 按 Enter 時
 * keydown 的 `key` 常是 `Unidentified`（keyCode 229），composer 的 `onKeyDown` 認不出來，
 * 文字就只是換行。瀏覽器保證會有的是 `beforeinput`：`inputType === 'insertLineBreak'`
 * （或 `insertParagraph`）。所以手機上再多接這一條，攔到就當送出。
 *
 * **要掛原生 listener，不能用 React 的 `onBeforeInput` prop**：那顆是 React 自己從
 * `textInput` / `keypress` / composition 合成出來的舊事件，`nativeEvent` 上根本沒有
 * `inputType`，判斷永遠不成立——這條路走過一次，手機上按 Enter 還是只換行
 * （2026-09-08 用 CDP 送 keyCode 229 的 Enter 實測，原生 `beforeinput` 收得到
 * `insertLineBreak`，React 的 `onBeforeInput` 收不到）。
 *
 * 有些軟鍵盤／IME 不走 `insertLineBreak`，而是把換行當**文字**送進來（`inputType` 是
 * `insertText`、`data` 剛好就是一個 `\n`），所以那一條也一起接：`data` 必須整個就是換行，
 * 貼上多行文字（`insertFromPaste`）與 IME 一次送出「文字＋換行」都不會誤判成送出。
 *
 * 只在 ≤640px 生效：桌面的 Shift+Enter 也是 `insertLineBreak`，接了就沒辦法換行。
 * 桌面的 Enter 走既有的 `onKeyDown`（它先 preventDefault，beforeinput 不會再來）。
 *
 * 另外把 `enterKeyHint="send"` 給 textarea，鍵盤右下角就畫成「送出」而不是換行鍵。
 */
export function useEnterToSend(ref: RefObject<HTMLTextAreaElement | null>, submit: () => void) {
  const phone = useMediaQuery(PHONE_QUERY)
  // submit 每次 render 都是新的 closure；用 ref 存最新的一份，effect 才不用跟著重掛。
  // 寫入放在無依賴的 effect 裡（不是 render 途中），beforeinput 一定發生在 commit 之後。
  const latest = useRef(submit)
  useEffect(() => {
    latest.current = submit
  })

  useEffect(() => {
    const el = ref.current
    if (!phone || !el) return
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
  }, [phone, ref])

  return { enterKeyHint: (phone ? 'send' : 'enter') as 'send' | 'enter' }
}
