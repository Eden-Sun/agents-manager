import { useSyncExternalStore } from 'react'

/*
 * 整個 app 只有這兩個斷點會改變**行為**（`docs/UI-DECISIONS.md`：手機 RWD 的兩個斷點）。
 * styles.css 裡另外那幾個（1240 / 1160 / 1080）只是把邊距與標籤一階一階收緊，沒有任何
 * JS 要跟著它們走，所以不寫在這裡。
 */

/**
 * 抽屜斷點。必須跟 styles.css 的 `@media (width <= 1024px)` 同一個數字：那個區塊把側欄
 * 變成離屏抽屜、把圖片暫存從右緣的欄改成底部的條。以下關著的側欄在畫面外，必須 inert；
 * 以上它是一欄永遠看得到的清單，必須可達。
 */
export const DRAWER_QUERY = '(max-width: 1024px)'

/**
 * 手機斷面。必須跟 styles.css 的 `@media (width <= 640px)` 同一個數字：那幾個區塊把標題列
 * 收成一行、額度縮成 chip、對話框變成全螢幕 sheet；JS 這邊用來換掉在窄螢幕會折兩行的長
 * 提示字、決定圖片暫存的預設收合，以及讓設定面板不要再去貼齒輪。
 */
export const PHONE_QUERY = '(max-width: 640px)'

/** `true` while the media query matches, re-rendering when that flips. */
export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (onChange) => {
      const mq = window.matchMedia(query)
      mq.addEventListener('change', onChange)
      return () => mq.removeEventListener('change', onChange)
    },
    () => window.matchMedia(query).matches,
    // No DOM during SSR/prerender: assume the desktop layout, which leaves the sidebar
    // reachable rather than hiding it from assistive tech.
    () => false,
  )
}
