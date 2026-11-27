
import { useSyncExternalStore } from 'react'

const KEY = 'am.mobilePreview.open'
const DEVICE_KEY = 'am.mobilePreview.device'
const EVENT = 'am:mobile-preview'

export type PreviewDevice = 'phone' | 'ipad-portrait' | 'ipad-landscape'

export const PREVIEW_DEVICES: Record<PreviewDevice, { w: number; h: number }> = {
  phone: { w: 393, h: 852 },
  'ipad-portrait': { w: 834, h: 1194 },
  'ipad-landscape': { w: 1194, h: 834 },
}

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

export function previewHeightCapFor(device: PreviewDevice, innerHeight: number): number {
  if (device === 'phone') return innerHeight * 0.56
  return Math.max(0, Math.min(innerHeight * 0.72, innerHeight - 290))
}

export function previewScaleFor(device: PreviewDevice, availWidth: number, availHeight?: number): number {
  const { w, h } = PREVIEW_DEVICES[device]
  if (device === 'phone' && (!Number.isFinite(availWidth) || availWidth <= 0)) return MOBILE_PREVIEW_SCALE
  const usableW = Math.max(120, Number.isFinite(availWidth) && availWidth > 0 ? availWidth : device === 'phone' ? 197 : 340)
  let scale = Math.min(usableW / w, device === 'phone' ? MOBILE_PREVIEW_SCALE : 0.75)
  if (availHeight && Number.isFinite(availHeight) && availHeight > 0) {
    scale = Math.min(scale, availHeight / h)
  }
  return Math.max(0.18, Math.round(scale * 1000) / 1000)
}

function read(): boolean {
  try {
    return window.localStorage.getItem(KEY) === '1'
  } catch {
    return false
  }
}

function parseDevice(raw: string | null): PreviewDevice {
  if (raw === 'ipad-portrait' || raw === 'ipad-landscape' || raw === 'phone') return raw
  return 'phone'
}

function readDevice(): PreviewDevice {
  try {
    return parseDevice(window.localStorage.getItem(DEVICE_KEY))
  } catch {
    return 'phone'
  }
}

let memOn: boolean | null = null
let memDevice: PreviewDevice | null = null

function current(): boolean {
  memOn ??= read()
  return memOn
}

function currentDevice(): PreviewDevice {
  memDevice ??= readDevice()
  return memDevice
}

function subscribe(onChange: () => void): () => void {
  const onStorage = () => {
    memOn = read()
    memDevice = readDevice()
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

export function useMobilePreviewDevice(): PreviewDevice {
  return useSyncExternalStore(subscribe, currentDevice, () => 'phone')
}

export function setMobilePreviewOpen(open: boolean): void {
  memOn = open
  try {
    window.localStorage.setItem(KEY, open ? '1' : '0')
  } catch {
  }
  window.dispatchEvent(new Event(EVENT))
}

export function setMobilePreviewDevice(device: PreviewDevice): void {
  memDevice = device
  try {
    window.localStorage.setItem(DEVICE_KEY, device)
  } catch {
  }
  window.dispatchEvent(new Event(EVENT))
}

export function resetMobilePreviewForTest(): void {
  memOn = null
  memDevice = null
}
