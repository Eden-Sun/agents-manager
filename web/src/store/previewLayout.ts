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

interface LayoutState {
  open: Record<string, true>
  width: number | null
}

/**
 * 記憶體這一份是 snapshot 的來源：寫入先更新它，儲存受阻（無痕、配額）時這一頁仍生效（issue #1809）。
 * 只在別的分頁寫入（`storage` 事件）時重讀 localStorage。物件只在內容真的變了才換新，snapshot 才會穩定。
 */
let state: LayoutState | null = null

function readState(): LayoutState {
  return { open: readOpenMap(), width: readStoredWidth() }
}

function current(): LayoutState {
  state ??= readState()
  return state
}

function subscribe(onChange: () => void): () => void {
  const onStorage = () => {
    state = readState()
    onChange()
  }
  window.addEventListener(EVENT, onChange)
  window.addEventListener('storage', onStorage)
  return () => {
    window.removeEventListener(EVENT, onChange)
    window.removeEventListener('storage', onStorage)
  }
}

/** 這顆 bot 的預覽欄是否展開；snapshot 是布林，不會因別顆 bot 的變動重繪。 */
export function usePreviewColOpen(botId: string | null): boolean {
  return useSyncExternalStore(
    subscribe,
    () => (botId ? current().open[botId] === true : false),
    () => false,
  )
}

export function usePreviewColWidth(): number | null {
  return useSyncExternalStore(
    subscribe,
    () => current().width,
    () => null,
  )
}

function persist(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value)
  } catch {
    // 無痕寫不進去：記憶體那份已更新，這一頁照樣生效。
  }
  window.dispatchEvent(new Event(EVENT))
}

export function setPreviewColOpen(botId: string, open: boolean): void {
  const map = { ...current().open }
  if (open) map[botId] = true
  else delete map[botId]
  state = { ...current(), open: map }
  persist(OPEN_KEY, JSON.stringify(map))
}

export function setPreviewColWidth(w: number): void {
  const width = Math.round(w)
  state = { ...current(), width }
  persist(WIDTH_KEY, String(width))
}

/** 測試用：丟掉記憶體快照，下一次讀取重新從 localStorage 來。 */
export function resetPreviewLayoutForTest(): void {
  state = null
}
