/**
 * Image attachments: uploaded first (`POST /bots/:id/attachments`), the send carries ids since agents only read text.
 * Bytes sit behind the UI token, so thumbnails are fetched into object URLs rather than a plain `src`.
 */

import { useCallback, useEffect, useLayoutEffect, useReducer, useRef, useState } from 'react'
import type { DragEvent } from 'react'
import * as api from '../api'
import type { Attachment } from '../api/types'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { MAX_BYTES, SHELF_MIME, shelfFilesFor } from '../store/shelf'
import { useStore } from '../store/store'
import './attachments.css'

export function isImageFile(f: File): boolean {
  return f.type.startsWith('image/')
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

/** One image waiting to be sent: uploading, ready (has an id), or failed. */
export interface Pending {
  key: string
  /** 名稱｜大小｜修改時間，擋重複用。 */
  fp: string
  name: string
  size: number
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
      const images = files.filter(isImageFile)
      const rejected = files.length - images.length
      if (rejected > 0) notify('error', `已略過 ${rejected} 個非圖片檔案，目前只支援圖片。`)
      if (!uploadTo) {
        if (images.length) notify('error', '找不到可接收圖片的 bot。')
        return
      }
      for (const file of images) {
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
        const previewUrl = URL.createObjectURL(file)
        urls.current.add(previewUrl)
        dispatch({ type: 'add', item: { key, fp, name: file.name || '圖片', size: file.size, previewUrl, id: null, error: null } })
        void api
          .uploadAttachment(uploadTo, file)
          .then((a: Attachment) => {
            dispatch({ type: 'uploaded', key, id: a.id })
          })
          .catch((e: unknown) => {
            const msg = e instanceof Error ? e.message : String(e)
            dispatch({ type: 'failed', key, error: msg })
            notify('error', `圖片「${file.name}」上傳失敗：${msg}`)
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

export function DropVeil({ label = '放開以附加圖片' }: { label?: string }) {
  return (
    <div className="drop-veil" aria-hidden="true">
      <span className="drop-veil-box">
        <ImageIcon />
        {label}
      </span>
    </div>
  )
}

/** Pending thumbnails; 40px 認不出是哪張，hover/focus 浮一張絕對定位的大預覽。 */
export function AttachTray({ items, onRemove, disabled }: { items: Pending[]; onRemove: (key: string) => void; disabled?: boolean }) {
  const [peek, setPeek] = useState<string | null>(null)
  if (items.length === 0) return null
  const peeked = items.find((it) => it.key === peek) ?? null
  const off = (key: string) => setPeek((k) => (k === key ? null : k))
  return (
    <div className="attach-tray" role="list" aria-label="待送出的圖片">
      {peeked ? (
        <div className="attach-peek" aria-hidden="true">
          <img src={peeked.previewUrl} alt="" />
          <span className="attach-peek-cap">
            {peeked.name} · {formatSize(peeked.size)}
          </span>
        </div>
      ) : null}
      {items.map((it) => (
        <div
          key={it.key}
          className={`attach-thumb${it.error ? ' failed' : ''}${it.id ? '' : ' uploading'}${peek === it.key ? ' peeking' : ''}`}
          role="listitem"
          onMouseEnter={() => setPeek(it.key)}
          onMouseLeave={() => off(it.key)}
          onFocus={() => setPeek(it.key)}
          onBlur={() => off(it.key)}
        >
          <img src={it.previewUrl} alt={it.name} />
          <span className="attach-thumb-name" title={`${it.name} · ${formatSize(it.size)}`}>
            {it.name}
          </span>
          {it.id ? null : it.error ? <span className="attach-thumb-state err">失敗</span> : <span className="attach-thumb-state">上傳中…</span>}
          <button
            type="button"
            className="attach-thumb-x"
            aria-label={`移除 ${it.name}`}
            title="移除"
            disabled={disabled}
            onClick={() => onRemove(it.key)}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  )
}

export function AttachPicker({ onFiles, disabled }: { onFiles: (files: File[]) => void; disabled?: boolean }) {
  const ref = useRef<HTMLInputElement>(null)
  return (
    <>
      <input
        ref={ref}
        type="file"
        accept="image/*"
        multiple
        hidden
        onChange={(e) => {
          onFiles(Array.from(e.target.files ?? []))
          e.target.value = ''
        }}
      />
      <button
        type="button"
        className="icon-btn attach-pick icon-tip"
        disabled={disabled}
        aria-label="附加圖片"
        data-tip="附加圖片 · 拖放 / 貼上"
        onClick={() => ref.current?.click()}
      >
        <ImageIcon />
      </button>
    </>
  )
}

export function ImageIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <rect x="1.75" y="2.75" width="12.5" height="10.5" rx="2" fill="none" stroke="currentColor" strokeWidth="1.5" />
      <circle cx="5.75" cy="6.25" r="1.15" fill="currentColor" />
      <path d="M2.5 11.5l3.25-3 2.5 2.25L11 8.25l2.5 2.75" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

/** Object URL per attachment id, shared across bubbles. */
const urlCache = new Map<string, Promise<string>>()
/** LRU 上限：每個 object URL 釘住最大 12 MB Blob；已解碼的 `<img>` 不受 revoke 影響。 */
const URL_CACHE_MAX = 64

function storedUrl(id: string): Promise<string> {
  let p = urlCache.get(id)
  if (p) {
    // Map 照插入順序迭代：重新插入就是「最近用過」。
    urlCache.delete(id)
    urlCache.set(id, p)
    return p
  }
  p = api.attachmentUrl(id).catch((e: unknown) => {
    urlCache.delete(id)
    throw e
  })
  urlCache.set(id, p)
  while (urlCache.size > URL_CACHE_MAX) {
    const oldest = urlCache.keys().next().value
    if (oldest === undefined) break
    const evicted = urlCache.get(oldest)
    urlCache.delete(oldest)
    void evicted?.then((u) => URL.revokeObjectURL(u)).catch(() => {})
  }
  return p
}

export function MessageAttachments({ items }: { items: Attachment[] }) {
  const [zoom, setZoom] = useState<Attachment | null>(null)
  if (items.length === 0) return null
  return (
    <>
      <div className="msg-attachments">
        {items.map((a) => (
          <StoredThumb key={a.id} item={a} onOpen={() => setZoom(a)} />
        ))}
      </div>
      {zoom ? <Lightbox item={zoom} onClose={() => setZoom(null)} /> : null}
    </>
  )
}

function StoredThumb({ item, onOpen }: { item: Attachment; onOpen: () => void }) {
  const url = useStoredUrl(item.id)
  return (
    <button
      type="button"
      className={`msg-attachment${url ? '' : ' loading'}`}
      title={`${item.name} · ${formatSize(item.size)}\n${item.path}`}
      onClick={onOpen}
    >
      {url ? <img src={url} alt={item.name} /> : <span className="msg-attachment-fallback">…</span>}
    </button>
  )
}

function useStoredUrl(id: string): string | null {
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => {
    let live = true
    storedUrl(id)
      .then((u) => {
        if (live) setUrl(u)
      })
      .catch(() => {
        if (live) setUrl(null)
      })
    return () => {
      live = false
    }
  }, [id])
  return url
}

function Lightbox({ item, onClose }: { item: Attachment; onClose: () => void }) {
  const url = useStoredUrl(item.id)
  const boxRef = useRef<HTMLDivElement>(null)
  const closeRef = useRef<HTMLButtonElement>(null)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // 讓 popover 先吃掉自己的 Escape。
      if (e.key !== 'Escape' || e.defaultPrevented) return
      onClose()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [onClose])

  useDialogFocus(true, boxRef, { initialFocus: () => closeRef.current })

  return (
    <div
      ref={boxRef}
      className="lightbox"
      role="dialog"
      aria-modal="true"
      aria-label={item.name}
      onClick={onClose}
    >
      <div className="lightbox-inner" onClick={(e) => e.stopPropagation()}>
        {url ? <img src={url} alt={item.name} /> : <div className="lightbox-loading">載入中…</div>}
        <div className="lightbox-bar">
          <span className="lightbox-name" title={item.path}>
            {item.name} · {formatSize(item.size)}
          </span>
          <code className="lightbox-path mono">{item.path}</code>
          <button type="button" className="mini-btn" ref={closeRef} onClick={onClose}>
            關閉
          </button>
        </div>
      </div>
    </div>
  )
}
