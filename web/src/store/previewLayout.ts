/**
 * 預覽欄（`components/PreviewColumn.tsx`，桌機 `.app` 最右邊那一欄）的版面偏好：是否展開（每顆 parent bot 各記一份）、拖過的寬度（全域一份）。
 * localStorage、不進 zustand：純版面偏好（同 `mobilePreview.ts`）。
 */

import { useSyncExternalStore } from 'react'

/** `{ [botId]: true }`；沒記＝收合。舊的全域旗標 `am.previewCol.open` 不遷移（各 bot 預設收合）。 */
const OPEN_KEY = 'am.previewCol.openBots'
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

/** 壞資料一律當空；只認值為 `true` 的鍵。 */
export function parseOpenMap(raw: string | null): Record<string, true> {
  if (!raw) return {}
  try {
    const v: unknown = JSON.parse(raw)
    if (typeof v !== 'object' || v === null || Array.isArray(v)) return {}
    return Object.fromEntries(Object.entries(v).filter(([, on]) => on === true)) as Record<string, true>
  } catch {
    return {}
  }
}

function readOpenMap(): Record<string, true> {
  try {
    return parseOpenMap(window.localStorage.getItem(OPEN_KEY))
  } catch {
    return {}
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

/** 這顆 bot 的預覽欄是否展開；snapshot 是布林，不會因別顆 bot 的變動重繪。 */
export function usePreviewColOpen(botId: string | null): boolean {
  return useSyncExternalStore(
    subscribe,
    () => (botId ? readOpenMap()[botId] === true : false),
    () => false,
  )
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

export function setPreviewColOpen(botId: string, open: boolean): void {
  const map = readOpenMap()
  if (open) map[botId] = true
  else delete map[botId]
  write(OPEN_KEY, JSON.stringify(map))
}
export const setPreviewColWidth = (w: number) => write(WIDTH_KEY, String(Math.round(w)))
