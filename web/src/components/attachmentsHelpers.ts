import { useCallback, useEffect, useLayoutEffect, useReducer, useRef, useState } from 'react'
import type { DragEvent } from 'react'
import * as api from '../api'
import type { Attachment } from '../api/types'
import { MAX_BYTES, SHELF_MIME, shelfFilesFor } from '../store/shelf'
import { useStore } from '../store/store'

/** One file waiting to be sent: uploading, ready (has an id), or failed. */
export interface Pending {
  key: string
  /** 名稱｜大小｜修改時間，擋重複用。 */
  fp: string
  name: string
  size: number
  /** An image is drawn as a thumbnail; anything else as an icon card. */
  isImage: boolean
  /** Local preview for an image; `''` for everything else (no blob held for nothing). */
  previewUrl: string
  id: string | null
  error: string | null
}

type PendingAction =
  | { type: 'add'; item: Pending }
  | { type: 'uploaded'; key: string; id: string }
  | { type: 'failed'; key: string; error: string }
  | { type: 'remove'; key: string }
  | { type: 'clear' }

function pendingReducer(items: Pending[], action: PendingAction): Pending[] {
  switch (action.type) {
    case 'add':
      return [...items, action.item]
    case 'uploaded':
      return items.map((it) => (it.key === action.key ? { ...it, id: action.id } : it))
    case 'failed':
      return items.map((it) => (it.key === action.key ? { ...it, error: action.error } : it))
    case 'remove':
      return items.filter((it) => it.key !== action.key)
    case 'clear':
      return []
  }
}

export function isImageFile(f: File): boolean {
  return f.type.startsWith('image/')
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

/** Composer attachment state for one draft. `uploadTo`: any group member works (attachments are project-scoped). */
export function useAttachments(uploadTo: string | null, resetKey: string | null) {
  const notify = useStore((s) => s.notify)
  const [items, dispatch] = useReducer(pendingReducer, [])
  const seq = useRef(0)
  const seen = useRef(new Set<string>())
  const urls = useRef(new Set<string>())
  const itemsRef = useRef<Pending[]>([])
  useEffect(() => {
    const held = urls.current
    return () => {
      for (const u of held) URL.revokeObjectURL(u)
    }
  }, [])

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
    for (const it of itemsRef.current) revokePreview(it.previewUrl)
    // 指紋一起清，否則換回原對話時同一張會被誤判重複。
    seen.current.clear()
    dispatch({ type: 'clear' })
  }, [revokePreview, resetKey])

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
        if (file.size > MAX_BYTES) {
          notify('error', `「${file.name}」有 ${formatSize(file.size)}，超過 ${formatSize(MAX_BYTES)} 上限。`)
          continue
        }
        const isImage = isImageFile(file)
        const previewUrl = isImage ? URL.createObjectURL(file) : ''
        if (previewUrl) urls.current.add(previewUrl)
        dispatch({ type: 'add', item: { key, fp, name: file.name || '檔案', size: file.size, isImage, previewUrl, id: null, error: null } })
        void api
          .uploadAttachment(uploadTo, file)
          .then((a: Attachment) => {
            dispatch({ type: 'uploaded', key, id: a.id })
          })
          .catch((e: unknown) => {
            const msg = e instanceof Error ? e.message : String(e)
            dispatch({ type: 'failed', key, error: msg })
            notify('error', `「${file.name}」上傳失敗：${msg}`)
          })
      }
    },
    [notify, uploadTo],
  )

  const remove = useCallback((key: string) => {
    const removed = items.find((it) => it.key === key)
    if (removed) {
      revokePreview(removed.previewUrl)
      seen.current.delete(removed.fp)
    }
    dispatch({ type: 'remove', key })
  }, [items, revokePreview])

  const clear = useCallback(() => {
    for (const it of items) revokePreview(it.previewUrl)
    seen.current.clear()
    dispatch({ type: 'clear' })
  }, [items, revokePreview])

  const ids = items.map((it) => it.id).filter((id): id is string => Boolean(id))
  const uploading = items.some((it) => !it.id && !it.error)

  return { items, add, remove, clear, ids, uploading }
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
