import { useEffect } from 'react'
import { PHONE_QUERY, useMediaQuery } from './useMediaQuery'

/**
 * 手機上鍵盤彈出／收起時把文件捲回 (0,0)。iOS Safari 在 focus 輸入框時會自己把頁面往上捲
 * 一截讓輸入框露出來，結果 header 被推出畫面、鍵盤收起後也不會捲回來；版面本來就用
 * 100dvh 在縮，不需要它幫忙。`visualViewport` 的 resize / scroll 就是鍵盤事件的可靠訊號。
 */
export function useViewportPin() {
  const phone = useMediaQuery(PHONE_QUERY)
  useEffect(() => {
    if (!phone) return
    const vv = window.visualViewport
    const pin = () => {
      if (window.scrollY !== 0 || window.scrollX !== 0) window.scrollTo(0, 0)
      if (document.documentElement.scrollTop !== 0) document.documentElement.scrollTop = 0
    }
    vv?.addEventListener('resize', pin)
    vv?.addEventListener('scroll', pin)
    const onFocus = () => setTimeout(pin, 50)
    window.addEventListener('focusin', onFocus)
    return () => {
      vv?.removeEventListener('resize', pin)
      vv?.removeEventListener('scroll', pin)
      window.removeEventListener('focusin', onFocus)
    }
  }, [phone])
}
