import { useEffect } from 'react'
import { DRAWER_QUERY, useMediaQuery } from './useMediaQuery'

export const SHEET_SELECTOR = '.modal, .bot-settings, .confirm-dialog'

export function revealInSheet(active: Pick<Element, 'closest' | 'scrollIntoView'> | null | undefined): boolean {
  if (!active || typeof active.closest !== 'function' || !active.closest(SHEET_SELECTOR)) return false
  active.scrollIntoView({ block: 'nearest' })
  return true
}

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
      setTimeout(apply, 50)
      setTimeout(apply, 250)
      setTimeout(() => {
        apply()
        revealInSheet(document.activeElement)
      }, 600)
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
