/**
 * 桌機手機預覽開關（`components/MobilePreview.tsx`）。localStorage 布林值、不進 zustand：
 * 純版面偏好，iframe 裡的 app 也是另一個 JS 世界。
 */

import { useSyncExternalStore } from 'react'

const KEY = 'am.mobilePreview.open'
/** 同 tab 內同步（`storage` 事件只送其他 tab）。 */
const EVENT = 'am:mobile-preview'

/** iframe 裡的同一個 app 不再開巢狀預覽；只在載入時讀，`routeSync` 改寫網址也不受影響。 */
export const IN_MOBILE_PREVIEW =
  typeof window !== 'undefined' && new URLSearchParams(window.location.search).has('mobilePreview')

/** iframe 視窗尺寸（iPhone 16 直向 CSS px）：媒體查詢照這個走，不能跟著顯示縮放縮小。 */
export const MOBILE_PREVIEW_W = 393
export const MOBILE_PREVIEW_H = 852

/** 顯示縮一半（2026-09-13 使用者：占桌機太多寬度）；scale 不改 iframe 視窗尺寸。 */
export const MOBILE_PREVIEW_SCALE = 0.5

function read(): boolean {
  try {
    return window.localStorage.getItem(KEY) === '1'
  } catch {
    return false
  }
}

function subscribe(onChange: () => void): () => void {
  window.addEventListener(EVENT, onChange)
  window.addEventListener('storage', onChange)
  return () => {
    window.removeEventListener(EVENT, onChange)
    window.removeEventListener('storage', onChange)
  }
}

/** 在預覽自己裡面一律 `false`。 */
export function useMobilePreviewOpen(): boolean {
  const on = useSyncExternalStore(subscribe, read, () => false)
  return on && !IN_MOBILE_PREVIEW
}

export function setMobilePreviewOpen(open: boolean): void {
  try {
    window.localStorage.setItem(KEY, open ? '1' : '0')
  } catch {
    // 無痕寫不進去：這一輪仍要生效，照樣發事件。
  }
  window.dispatchEvent(new Event(EVENT))
}
