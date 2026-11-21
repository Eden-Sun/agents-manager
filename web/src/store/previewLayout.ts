
import { useSyncExternalStore } from 'react'

const OPEN_KEY = 'am.previewCol.openBots'
const WIDTH_KEY = 'am.previewCol.width'
const EVENT = 'am:preview-col'

export const PREVIEW_COL_MIN = 320
export const PREVIEW_RAIL_W = 36

export function defaultPreviewWidth(viewport: number): number {
  return clampPreviewWidth(Math.round(viewport * 0.4), viewport)
}

export function clampPreviewWidth(w: number, viewport: number): number {
  const max = Math.max(PREVIEW_COL_MIN, Math.floor(viewport * 0.5))
  return Math.min(max, Math.max(PREVIEW_COL_MIN, Math.round(w)))
}

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

export function resetPreviewLayoutForTest(): void {
  state = null
}
