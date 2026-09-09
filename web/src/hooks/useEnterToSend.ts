import { PHONE_QUERY, useMediaQuery } from './useMediaQuery'

/**
 * Enter 要不要送出，看裝置：
 *
 * - **桌面**：Enter 送出、Shift+Enter 換行（既有行為，各 composer 的 `onKeyDown` 自己做）。
 * - **手機（≤640px）**：Enter 只換行、送出靠按鈕（2026-09-09 使用者決定）。軟鍵盤上沒有
 *   Shift+Enter 這種組合，Enter 一按就送會讓多行訊息打不出來；而且 iOS 的注音／拼音在
 *   選字時也按 Enter，誤送過好幾次。`enterKeyHint` 跟著改回「換行」的樣子。
 *
 * 之前（2026-09-08）走過相反的路——手機 Enter 送出、接 `beforeinput` 的 `insertLineBreak`
 * 補 Android 的 keyCode 229——那套已拿掉；別再加回來，先問使用者。
 */
export function useEnterToSend() {
  const phone = useMediaQuery(PHONE_QUERY)
  return {
    /** 各 composer 的 `onKeyDown` 用這個決定 Enter 要不要送。 */
    enterSends: !phone,
    props: { enterKeyHint: 'enter' as const },
  }
}
