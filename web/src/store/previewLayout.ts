/**
 * 預覽欄（`components/PreviewColumn.tsx`，桌機 `.app` 最右邊那一欄）的版面偏好：是否展開、拖過的寬度。
 * localStorage、不進 zustand：純版面偏好（同 `mobilePreview.ts`）。
 */

import { useSyncExternalStore } from 'react'

const OPEN_KEY = 'am.previewCol.open'
const WIDTH_KEY = 'am.previewCol.width'
const EVENT = 'am:preview-col'

export const PREVIEW_COL_MIN = 320
/** 收合時的窄條寬。 */
export const PREVIEW_RAIL_W = 36

/** 預設寬：視窗的 40%，落在 min 與 max 之間。 */
export function defaultPreviewWidth(viewport: number): number {
  return clampPreviewWidth(Math.round(viewport * 0.4), viewport)
}

/** 上限留給側欄＋主面板：視窗的 50%（再寬主面板就擠得沒法用）。窄到放不下 min 時 min 優先。 */
export function clampPreviewWidth(w: number, viewport: number): number {
  const max = Math.max(PREVIEW_COL_MIN, Math.floor(viewport * 0.5))
  return Math.min(max, Math.max(PREVIEW_COL_MIN, Math.round(w)))
}

function readOpen(): boolean {
  try {
    return window.localStorage.getItem(OPEN_KEY) === '1'
  } catch {
    return false
  }
}

/** 沒存過（或壞資料）＝null，由呼叫端用預設值。 */
export function readStoredWidth(): number | null {
  try {
    const n = Number(window.localStorage.getItem(WIDTH_KEY))
    return Number.isFinite(n) && n > 0 ? n : null
  } catch {
    return null
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

export function usePreviewColOpen(): boolean {
  return useSyncExternalStore(subscribe, readOpen, () => false)
}

export function usePreviewColWidth(): number | null {
  return useSyncExternalStore(subscribe, readStoredWidth, () => null)
}

function write(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value)
  } catch {
    // 無痕寫不進去：這一輪仍要生效，照樣發事件。
  }
  window.dispatchEvent(new Event(EVENT))
}

export const setPreviewColOpen = (open: boolean) => write(OPEN_KEY, open ? '1' : '0')
export const setPreviewColWidth = (w: number) => write(WIDTH_KEY, String(Math.round(w)))
