/**
 * 桌機手機預覽的開關狀態（見 `components/MobilePreview.tsx`）。
 *
 * 只是一個 localStorage 布林值，不進 zustand：它不影響任何資料，純粹是本機的版面偏好，
 * 而且 iframe 裡那份 app 是另一個 JS 世界，共用 store 沒有意義。
 */

import { useSyncExternalStore } from 'react'

const KEY = 'am.mobilePreview.open'
/** 同一個 tab 內跨元件同步（`storage` 事件只會送到其他 tab）。 */
const EVENT = 'am:mobile-preview'

/**
 * iframe 裡載入的是同一個 app，所以它自己也會想再開一個預覽——用網址參數把它關掉。
 * 只在模組載入時讀一次：`routeSync` 之後改寫網址也不該讓預覽突然長出巢狀預覽。
 */
export const IN_MOBILE_PREVIEW =
  typeof window !== 'undefined' && new URLSearchParams(window.location.search).has('mobilePreview')

/**
 * 預覽 iframe 的**視窗**尺寸：iPhone 16 直向的 CSS 像素（393×852）。
 *
 * 這是 iframe 裡那份 app 真正量到的視窗——媒體查詢、`100dvh`、安全區都照這個走，所以它不能
 * 因為畫面上顯示得比較小就跟著縮（縮了就不是在預覽手機版面，而是在預覽一個沒人用的尺寸）。
 */
export const MOBILE_PREVIEW_W = 393
export const MOBILE_PREVIEW_H = 852

/**
 * 畫面上縮到一半再擺（2026-09-13 使用者：那一格占掉桌機太多寬度）。
 *
 * `transform: scale()` 只改「畫多大」，不改 iframe 自己的視窗尺寸，所以量到的還是 393×852 的
 * 版面；右欄需要的寬度則從 406px 降到約 213px。
 */
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

/** 預覽是否開著。在預覽自己裡面一律 `false`。 */
export function useMobilePreviewOpen(): boolean {
  const on = useSyncExternalStore(subscribe, read, () => false)
  return on && !IN_MOBILE_PREVIEW
}

export function setMobilePreviewOpen(open: boolean): void {
  try {
    window.localStorage.setItem(KEY, open ? '1' : '0')
  } catch {
    // 無痕視窗寫不進去：這一輪仍然要生效，所以照樣發事件。
  }
  window.dispatchEvent(new Event(EVENT))
}
