import { t } from '../lib/i18n'

import type { Attachment } from '../api/types'
import { isAbortError } from '../api/transport'
import type { UploadOptions } from '../api/transport'
import { MAX_BYTES } from '../store/shelf'

export interface Pending {
  key: string
  fp: string
  name: string
  size: number
  originalSize?: number
  isImage: boolean
  previewUrl: string
  compressing: boolean
  loaded: number
  id: string | null
  error: string | null
  retryable: boolean
}

export function restoredAttachment(attachment: Attachment, key: string): Pending {
  return {
    key,
    fp: `restored:${attachment.id}`,
    name: attachment.name,
    size: attachment.size,
    isImage: false,
    previewUrl: '',
    compressing: false,
    loaded: attachment.size,
    id: attachment.id,
    error: null,
    retryable: false,
  }
}

export type PendingAction =
  | { type: 'add'; item: Pending }
  | { type: 'restore'; item: Pending }
  | { type: 'compressed'; key: string; name: string; size: number }
  | { type: 'uploading'; key: string }
  | { type: 'progress'; key: string; loaded: number }
  | { type: 'uploaded'; key: string; id: string }
  | { type: 'failed'; key: string; error: string; retryable: boolean }
  | { type: 'retry'; key: string }
  | { type: 'remove'; key: string }
  | { type: 'clear' }

export type AddVerdict = 'accept' | 'duplicate' | 'too-large'

export function admitFile(seen: Set<string>, fp: string, size: number, canCompress: boolean): AddVerdict {
  if (seen.has(fp)) return 'duplicate'
  if (size > MAX_BYTES && !canCompress) return 'too-large'
  seen.add(fp)
  return 'accept'
}

export function pendingReducer(items: Pending[], action: PendingAction): Pending[] {
  const patch = (key: string, f: (it: Pending) => Pending) => items.map((it) => (it.key === key ? f(it) : it))
  switch (action.type) {
    case 'add':
      return [...items, action.item]
    case 'restore':
      return items.some((it) => (action.item.id && it.id === action.item.id) || it.key === action.item.key) ? items : [...items, action.item]
    case 'compressed':
      return patch(action.key, (it) => ({ ...it, name: action.name, originalSize: it.size, size: action.size }))
    case 'uploading':
      return patch(action.key, (it) => ({ ...it, compressing: false, loaded: 0 }))
    case 'progress':
      return patch(action.key, (it) => ({ ...it, loaded: Math.min(it.size, Math.max(0, action.loaded)) }))
    case 'uploaded':
      return patch(action.key, (it) => ({ ...it, id: action.id, loaded: it.size }))
    case 'failed':
      return patch(action.key, (it) => ({ ...it, compressing: false, error: action.error, retryable: action.retryable }))
    case 'retry':
      return patch(action.key, (it) => ({ ...it, error: null, loaded: 0 }))
    case 'remove':
      return items.filter((it) => it.key !== action.key)
    case 'clear':
      return []
  }
}

class TooLargeError extends Error {}

export interface UploadDeps {
  compress: (file: File) => Promise<File>
  upload: (file: File, opts: UploadOptions) => Promise<Attachment>
  dispatch: (action: PendingAction) => void
  onPrepared: (file: File) => void
  onError: (name: string, message: string) => void
  formatSize: (bytes: number) => string
}

export async function runUpload(deps: UploadDeps, key: string, file: File, compress: boolean, signal: AbortSignal): Promise<void> {
  try {
    let out = file
    if (compress) {
      out = await deps.compress(file)
      if (signal.aborted) return
      if (out.size > MAX_BYTES) {
        throw new TooLargeError(t('有 {size}，超過 {max} 上限', { size: deps.formatSize(out.size), max: deps.formatSize(MAX_BYTES) }))
      }
      if (out !== file) deps.dispatch({ type: 'compressed', key, name: out.name, size: out.size })
      deps.onPrepared(out)
    }
    deps.dispatch({ type: 'uploading', key })
    const worthDrawing = progressGate()
    const a = await deps.upload(out, {
      signal,
      onProgress: (loaded, total) => {
        if (!signal.aborted && worthDrawing(loaded, total)) deps.dispatch({ type: 'progress', key, loaded })
      },
    })
    if (signal.aborted) return
    deps.dispatch({ type: 'uploaded', key, id: a.id })
  } catch (e: unknown) {
    if (signal.aborted || isAbortError(e)) return
    const msg = e instanceof Error ? e.message : String(e)
    deps.dispatch({ type: 'failed', key, error: msg, retryable: !(e instanceof TooLargeError) })
    deps.onError(file.name, msg)
  }
}

export function progressGate(): (loaded: number, total: number) => boolean {
  let shown = ''
  return (loaded, total) => {
    const next = progressLabel(loaded, total)
    if (next === shown) return false
    shown = next
    return true
  }
}

export function uploadPercent(loaded: number, total: number): number {
  if (total <= 0) return 0
  return Math.max(0, Math.min(100, Math.floor((loaded * 100) / total)))
}

export function progressLabel(loaded: number, total: number): string {
  const pct = uploadPercent(loaded, total)
  if (total < 1024) return `${loaded} / ${total} B · ${pct}%`
  if (Math.round(total / 1024) < 1024) return `${Math.round(loaded / 1024)} / ${Math.round(total / 1024)} KB · ${pct}%`
  const mb = (n: number) => {
    const v = n / (1024 * 1024)
    return Math.round(v * 10) / 10 === 10 ? '10' : v.toFixed(1)
  }
  return `${mb(loaded)} / ${mb(total)} MB · ${pct}%`
}
