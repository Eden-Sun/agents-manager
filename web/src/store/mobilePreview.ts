
import { useSyncExternalStore } from 'react'

const KEY = 'am.mobilePreview.open'
const EVENT = 'am:mobile-preview'

export const IN_MOBILE_PREVIEW = inMobilePreview()

function inMobilePreview(): boolean {
  try {
    return typeof window !== 'undefined' && new URLSearchParams(window.location?.search ?? '').has('mobilePreview')
  } catch {
    return false
  }
}

export function writeShared(write: () => void, inPreview: boolean = IN_MOBILE_PREVIEW): void {
  if (!inPreview) write()
}

export function surveyDraftAllowed(survey: boolean, inPreview: boolean = IN_MOBILE_PREVIEW): boolean {
  return survey && !inPreview
}

export const MOBILE_PREVIEW_W = 393
export const MOBILE_PREVIEW_H = 852

export const MOBILE_PREVIEW_SCALE = 0.5

function read(): boolean {
  try {
    return window.localStorage.getItem(KEY) === '1'
  } catch {
    return false
  }
}

let memOn: boolean | null = null

function current(): boolean {
  memOn ??= read()
  return memOn
}

function subscribe(onChange: () => void): () => void {
  const onStorage = () => {
    memOn = read()
    onChange()
  }
  window.addEventListener(EVENT, onChange)
  window.addEventListener('storage', onStorage)
  return () => {
    window.removeEventListener(EVENT, onChange)
    window.removeEventListener('storage', onStorage)
  }
}

export function useMobilePreviewOpen(): boolean {
  const on = useSyncExternalStore(subscribe, current, () => false)
  return on && !IN_MOBILE_PREVIEW
}

export function setMobilePreviewOpen(open: boolean): void {
  memOn = open
  try {
    window.localStorage.setItem(KEY, open ? '1' : '0')
  } catch {
  }
  window.dispatchEvent(new Event(EVENT))
}

export function resetMobilePreviewForTest(): void {
  memOn = null
}
