import { useEffect } from 'react'
import { DRAWER_QUERY, useMediaQuery } from './useMediaQuery'

/**
 * iOS Safari 鍵盤不縮 layout viewport（`100dvh`、`resizes-content` 都不理），2026-09-09 使用者
 * 打字時看不到字。只有 `visualViewport.height` 會變，寫進 `--vvh` 給手機版高度用，並捲回 (0,0)。
 *
 * 門檻是 `DRAWER_QUERY`（≤1024px）不是 `PHONE_QUERY`（issue #548）：iPadOS Safari 一樣不縮 layout
 * viewport，而 641–1024px 的抽屜版面（iPad 直式 768／820、iPad mini 744）原本沒有 `--vvh`，
 * 鍵盤一彈出來就把底部輸入列蓋住。
 */
export function useViewportPin() {
  const narrow = useMediaQuery(DRAWER_QUERY)
  useEffect(() => {
    if (!narrow) return
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
  }, [narrow])
}
