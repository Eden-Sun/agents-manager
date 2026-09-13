import { PHONE_QUERY, useMediaQuery } from './useMediaQuery'

/**
 * 桌面 Enter 送出；手機 Enter 只換行、送出靠按鈕（2026-09-09 使用者決定：軟鍵盤無 Shift+Enter、
 * iOS 選字 Enter 誤送）。2026-09-08 手機 Enter 送出那套已拿掉，別加回來，先問使用者。
 */
export function useEnterToSend() {
  const phone = useMediaQuery(PHONE_QUERY)
  return {
    enterSends: !phone,
    props: { enterKeyHint: 'enter' as const },
  }
}
