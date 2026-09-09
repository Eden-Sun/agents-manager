import { useEffect } from 'react'
import { PHONE_QUERY, useMediaQuery } from './useMediaQuery'

/**
 * 手機鍵盤彈出時把版面縮到「真的看得到」的高度，並把文件捲回 (0,0)。
 *
 * iOS Safari 的鍵盤**不會**縮 layout viewport：`100dvh`、`interactive-widget=resizes-content`
 * 它都不理，鍵盤只是蓋在畫面上、再把文件往上捲一截讓輸入框露出來——結果 header 被推走、
 * 底部的輸入框又常常整個在鍵盤底下（2026-09-09 使用者截圖：打字時看不到自己打的字）。
 * 唯一會跟著鍵盤變的是 `visualViewport.height`，所以把它寫成 `--vvh`，≤640px 的
 * `html / body / #root` 高度吃這個變數；鍵盤一出來版面就縮，輸入框自然貼在鍵盤上緣。
 * 鍵盤收起、網址列伸縮也走同一條路。
 */
export function useViewportPin() {
  const phone = useMediaQuery(PHONE_QUERY)
  useEffect(() => {
    if (!phone) return
    const vv = window.visualViewport
    const root = document.documentElement
    const apply = () => {
      if (vv) root.style.setProperty('--vvh', `${Math.round(vv.height)}px`)
      if (window.scrollY !== 0 || window.scrollX !== 0) window.scrollTo(0, 0)
      if (root.scrollTop !== 0) root.scrollTop = 0
    }
    apply()
    const onFocus = () => {
      // iOS 在 focus 之後才動 viewport，多補幾次直到它安定。
      setTimeout(apply, 50)
      setTimeout(apply, 250)
      setTimeout(apply, 600)
    }
    vv?.addEventListener('resize', apply)
    vv?.addEventListener('scroll', apply)
    window.addEventListener('focusin', onFocus)
    window.addEventListener('focusout', onFocus)
    return () => {
      vv?.removeEventListener('resize', apply)
      vv?.removeEventListener('scroll', apply)
      window.removeEventListener('focusin', onFocus)
      window.removeEventListener('focusout', onFocus)
      root.style.removeProperty('--vvh')
    }
  }, [phone])
}
