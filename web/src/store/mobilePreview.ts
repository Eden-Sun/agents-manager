/**
 * 桌機手機預覽開關（`components/MobilePreview.tsx`）。localStorage 布林值、不進 zustand：
 * 純版面偏好，iframe 裡的 app 也是另一個 JS 世界。
 */

import { useSyncExternalStore } from 'react'

const KEY = 'am.mobilePreview.open'
/** 同 tab 內同步（`storage` 事件只送其他 tab）。 */
const EVENT = 'am:mobile-preview'

/**
 * iframe 裡的同一個 app 不再開巢狀預覽；只在載入時讀，`routeSync` 改寫網址也不受影響。
 * 讀不到網址就當不是預覽：`store.ts` 會 import 這個模組，沒有 `location` 的跑道（測試 harness）
 * 不能因此整包炸在 import。
 */
export const IN_MOBILE_PREVIEW = inMobilePreview()

function inMobilePreview(): boolean {
  try {
    return typeof window !== 'undefined' && new URLSearchParams(window.location?.search ?? '').has('mobilePreview')
  } catch {
    return false
  }
}

/**
 * 預覽裡那份 app 是同一個 origin 的第二個 client，跟主畫面共用 localStorage，卻各自拿記憶體裡的**整份** map 覆寫
 * （草稿、游標、選取、未讀）：在預覽裡打一個字，主畫面其他 bot 的草稿就從 localStorage 被洗掉（review3 c1 L8）。
 * 預覽只讀不寫。
 */
export function writeShared(write: () => void, inPreview: boolean = IN_MOBILE_PREVIEW): void {
  if (!inPreview) write()
}

/**
 * 多分頁問卷要不要走「先預載、離線作答」。預載會**自己**在分頁間送 ←／→；預載鎖是 module 層的，跨不過 iframe，
 * 主畫面和預覽同時預載同一顆 bot 時導覽鍵互相插隊——分頁數錯、草稿對不上題目（review3 c1 L8）。
 * 預覽裡退回即時模式：只有使用者點了才送鍵。
 */
export function surveyDraftAllowed(survey: boolean, inPreview: boolean = IN_MOBILE_PREVIEW): boolean {
  return survey && !inPreview
}

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
