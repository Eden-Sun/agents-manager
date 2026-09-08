import type { FormEvent } from 'react'
import { PHONE_QUERY, useMediaQuery } from './useMediaQuery'

/**
 * 手機軟鍵盤的 Enter 不會送出（2026-09-08 使用者回報）。Android Chrome / Gboard 按 Enter 時
 * keydown 的 `key` 常是 `Unidentified`（keyCode 229），composer 的 `onKeyDown` 認不出來，
 * 文字就只是換行。瀏覽器保證會有的是 `beforeinput`：`inputType === 'insertLineBreak'`
 * （或 `insertParagraph`）。所以手機上再多接這一條，攔到就當送出。
 *
 * 只在 ≤640px 生效：桌面的 Shift+Enter 也是 `insertLineBreak`，接了就沒辦法換行。
 * 桌面的 Enter 走既有的 `onKeyDown`（它先 preventDefault，beforeinput 不會再來）。
 *
 * 另外把 `enterKeyHint="send"` 給 textarea，鍵盤右下角就畫成「送出」而不是換行鍵。
 */
export function useEnterToSend(submit: () => void) {
  const phone = useMediaQuery(PHONE_QUERY)
  return {
    enterKeyHint: (phone ? 'send' : 'enter') as 'send' | 'enter',
    onBeforeInput: (e: FormEvent<HTMLTextAreaElement>) => {
      if (!phone) return
      const ev = e.nativeEvent as InputEvent
      if (ev.isComposing) return
      if (ev.inputType === 'insertLineBreak' || ev.inputType === 'insertParagraph') {
        e.preventDefault()
        submit()
      }
    },
  }
}
