import { useCallback, useEffect, useLayoutEffect, useReducer, useRef, useState } from 'react'
import type { DragEvent } from 'react'
import * as api from '../api'
import { compressible, compressImage } from '../lib/imageCompress'
import { MAX_BYTES, SHELF_MIME, shelfFilesFor } from '../store/shelf'
import { useStore } from '../store/store'
import { pendingReducer, runUpload } from './attachmentUpload'
import type { Pending, UploadDeps } from './attachmentUpload'

export type { Pending } from './attachmentUpload'

export function isImageFile(f: File): boolean {
  return f.type.startsWith('image/')
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

/** 卡片上的大小：壓過的連原檔一起寫，看得出省了多少。 */
export function sizeLabel(size: number, originalSize?: number): string {
  return originalSize && originalSize !== size ? `${formatSize(size)}（原 ${formatSize(originalSize)}）` : formatSize(size)
}

/** Composer attachment state for one draft. `uploadTo`: any group member works (attachments are project-scoped). */
export function useAttachments(uploadTo: string | null, resetKey: string | null) {
  const notify = useStore((s) => s.notify)
  const [items, dispatch] = useReducer(pendingReducer, [])
  const seq = useRef(0)
  const seen = useRef(new Set<string>())
  const urls = useRef(new Set<string>())
  const itemsRef = useRef<Pending[]>([])
  /** 每張卡片正在跑的那一次上傳；× 移除／清空／換對話都要真的 abort（issue #435）。 */
  const inflight = useRef(new Map<string, AbortController>())
  /** 重試要傳的檔：`prepared` 表示已經壓過了，重試直接傳這份。 */
  const sources = useRef(new Map<string, { file: File; prepared: boolean }>())

  const abortOne = useCallback((key: string) => {
    inflight.current.get(key)?.abort()
    inflight.current.delete(key)
    sources.current.delete(key)
  }, [])

  const abortAll = useCallback(() => {
    for (const c of inflight.current.values()) c.abort()
    inflight.current.clear()
    sources.current.clear()
  }, [])

  useEffect(() => {
    const held = urls.current
    return () => {
      abortAll()
      for (const u of held) URL.revokeObjectURL(u)
    }
  }, [abortAll])

  const revokePreview = useCallback((previewUrl: string) => {
    if (!previewUrl) return
    URL.revokeObjectURL(previewUrl)
    urls.current.delete(previewUrl)
  }, [])

  useLayoutEffect(() => {
    itemsRef.current = items
  }, [items])

  // Attachments belong to the conversation, not necessarily the bot that received the upload.
  useLayoutEffect(() => {
    abortAll()
    for (const it of itemsRef.current) revokePreview(it.previewUrl)
    // 指紋一起清，否則換回原對話時同一張會被誤判重複。
    seen.current.clear()
    dispatch({ type: 'clear' })
  }, [abortAll, revokePreview, resetKey])

  const start = useCallback(
    (to: string, key: string, file: File, compress: boolean) => {
      const ctl = new AbortController()
      inflight.current.set(key, ctl)
      const deps: UploadDeps = {
        compress: compressImage,
        upload: (f, opts) => api.uploadAttachment(to, f, opts),
        dispatch,
        onPrepared: (f) => {
          if (!ctl.signal.aborted) sources.current.set(key, { file: f, prepared: true })
        },
        onError: (name, msg) => notify('error', `「${name}」上傳失敗：${msg}`),
        formatSize,
      }
      void runUpload(deps, key, file, compress, ctl.signal).finally(() => {
        if (inflight.current.get(key) === ctl) inflight.current.delete(key)
      })
    },
    [notify],
  )

  const add = useCallback(
    (files: File[]) => {
      if (!uploadTo) {
        if (files.length) notify('error', '找不到可接收檔案的 bot。')
        return
      }
      for (const file of files) {
        // 同一張不重複放入（2026-09-09 使用者：同一張暫存會重複放入對話）。
        const fp = `${file.name}|${file.size}|${file.lastModified}`
        if (seen.current.has(fp)) continue
        seen.current.add(fp)
        seq.current += 1
        const key = `a${seq.current}`
        // 圖片先壓再比上限（`lib/imageCompress.ts`）：手機原圖超過上限，壓完多半放得下。
        const compress = compressible(file.type)
        if (file.size > MAX_BYTES && !compress) {
          notify('error', `「${file.name}」有 ${formatSize(file.size)}，超過 ${formatSize(MAX_BYTES)} 上限。`)
          continue
        }
        const isImage = isImageFile(file)
        const previewUrl = isImage ? URL.createObjectURL(file) : ''
        if (previewUrl) urls.current.add(previewUrl)
        const item: Pending = {
          key, fp, name: file.name || '檔案', size: file.size, isImage, previewUrl,
          compressing: compress, loaded: 0, id: null, error: null, retryable: true,
        }
        dispatch({ type: 'add', item })
        sources.current.set(key, { file, prepared: !compress })
        start(uploadTo, key, file, compress)
      }
    },
    [notify, start, uploadTo],
  )

  /** 失敗的卡片再傳一次：壓過的直接傳壓好的那份。 */
  const retry = useCallback(
    (key: string) => {
      const src = sources.current.get(key)
      const it = items.find((x) => x.key === key)
      if (!uploadTo || !src || !it?.error || !it.retryable) return
      dispatch({ type: 'retry', key })
      start(uploadTo, key, src.file, !src.prepared)
    },
    [items, start, uploadTo],
  )

  const remove = useCallback((key: string) => {
    abortOne(key)
    const removed = items.find((it) => it.key === key)
    if (removed) {
      revokePreview(removed.previewUrl)
      seen.current.delete(removed.fp)
    }
    dispatch({ type: 'remove', key })
  }, [abortOne, items, revokePreview])

  const clear = useCallback(() => {
    abortAll()
    for (const it of items) revokePreview(it.previewUrl)
    seen.current.clear()
    dispatch({ type: 'clear' })
  }, [abortAll, items, revokePreview])

  const ids = items.map((it) => it.id).filter((id): id is string => Boolean(id))
  const uploading = items.some((it) => !it.id && !it.error)

  return { items, add, remove, retry, clear, ids, uploading }
}

/** Drop target for OS files and shelf drags (key only; the shelf never uploads, so bytes go to the bot dropped on). */
export function useDropTarget(onFiles: (files: File[]) => void, disabled?: boolean) {
  const [over, setOver] = useState(false)
  // dragenter/dragleave fire per child; count so the overlay does not flicker.
  const depth = useRef(0)

  const shelfKeys = (e: DragEvent): string[] =>
    (e.dataTransfer?.getData(SHELF_MIME) || '')
      .split(',')
      .map((k) => k.trim())
      .filter(Boolean)

  const hasFiles = (e: DragEvent) => {
    const types = Array.from(e.dataTransfer?.types ?? [])
    return types.includes('Files') || types.includes(SHELF_MIME)
  }

  // Drags from outside the window (macOS screenshot thumbnail) may never send `dragleave`; reset at window level.
  useEffect(() => {
    const reset = () => {
      depth.current = 0
      setOver(false)
    }
    const onWindowLeave = (e: globalThis.DragEvent) => {
      if (e.relatedTarget === null) reset()
    }
    window.addEventListener('drop', reset)
    window.addEventListener('dragend', reset)
    window.addEventListener('dragleave', onWindowLeave)
    return () => {
      window.removeEventListener('drop', reset)
      window.removeEventListener('dragend', reset)
      window.removeEventListener('dragleave', onWindowLeave)
    }
  }, [])

  const props = {
    onDragEnter: (e: DragEvent) => {
      if (disabled || !hasFiles(e)) return
      e.preventDefault()
      depth.current += 1
      setOver(true)
    },
    onDragOver: (e: DragEvent) => {
      if (disabled || !hasFiles(e)) return
      e.preventDefault()
      if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy'
    },
    onDragLeave: (e: DragEvent) => {
      if (disabled || !hasFiles(e)) return
      e.preventDefault()
      depth.current = Math.max(0, depth.current - 1)
      if (depth.current === 0) setOver(false)
    },
    onDrop: (e: DragEvent) => {
      if (disabled || !hasFiles(e)) return
      e.preventDefault()
      depth.current = 0
      setOver(false)
      const keys = shelfKeys(e)
      onFiles(keys.length ? shelfFilesFor(keys) : Array.from(e.dataTransfer?.files ?? []))
    },
  }

  return { over, props }
}
