/**
 * Image attachments in the composer and in the timeline.
 *
 * A CLI agent only ever receives text, so a dropped image is uploaded first
 * (`POST /bots/:id/attachments`, which lands the file on the bot's host) and the send then
 * carries the returned ids; the daemon appends their paths to what the agent reads. Here
 * that means: a tray of pending thumbnails under the textarea, drop / paste / file-picker
 * as three ways in, and the thumbnails again on the sent message.
 *
 * Bytes sit behind the UI token, so every thumbnail is fetched and turned into an object
 * URL rather than pointed at with a plain `src`.
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import type { DragEvent } from 'react'
import * as api from '../api'
import type { Attachment } from '../api/types'
import { useFocusTrap } from '../hooks/useFocusTrap'
import { MAX_BYTES, SHELF_MIME, shelfFilesFor } from '../store/shelf'
import { useStore } from '../store/store'

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
  name: string
  size: number
  /** Local preview, available before the upload finishes. */
  previewUrl: string
  /** Set once the daemon has stored it; this is what the send carries. */
  id: string | null
  error: string | null
}

/**
 * Composer-side attachment state for one draft (a bot chat or a project group chat).
 * `uploadTo` is the bot whose host receives the file — for a group send any member will
 * do, since the daemon scopes attachments to the project they share.
 */
export function useAttachments(uploadTo: string | null) {
  const notify = useStore((s) => s.notify)
  const [items, setItems] = useState<Pending[]>([])
  const seq = useRef(0)
  // Object URLs are revoked on unmount only: a thumbnail must stay valid while it is shown.
  const urls = useRef<string[]>([])
  useEffect(() => {
    const held = urls.current
    return () => {
      for (const u of held) URL.revokeObjectURL(u)
    }
  }, [])

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
        seq.current += 1
        const key = `a${seq.current}`
        if (file.size > MAX_BYTES) {
          notify('error', `「${file.name}」有 ${formatSize(file.size)}，超過 ${formatSize(MAX_BYTES)} 上限。`)
          continue
        }
        const previewUrl = URL.createObjectURL(file)
        urls.current.push(previewUrl)
        setItems((prev) => [...prev, { key, name: file.name || '圖片', size: file.size, previewUrl, id: null, error: null }])
        void api
          .uploadAttachment(uploadTo, file)
          .then((a: Attachment) => {
            setItems((prev) => prev.map((it) => (it.key === key ? { ...it, id: a.id } : it)))
          })
          .catch((e: unknown) => {
            const msg = e instanceof Error ? e.message : String(e)
            setItems((prev) => prev.map((it) => (it.key === key ? { ...it, error: msg } : it)))
            notify('error', `圖片「${file.name}」上傳失敗：${msg}`)
          })
      }
    },
    [notify, uploadTo],
  )

  const remove = useCallback((key: string) => {
    setItems((prev) => prev.filter((it) => it.key !== key))
  }, [])

  const clear = useCallback(() => setItems([]), [])

  /** Ids to send; empty while anything is still uploading. */
  const ids = items.map((it) => it.id).filter((id): id is string => Boolean(id))
  const uploading = items.some((it) => !it.id && !it.error)

  return { items, add, remove, clear, ids, uploading }
}

/**
 * Drag-and-drop wiring for a composer area: `props` on the drop target, plus the overlay.
 *
 * Two kinds of drag land here and both end up in the same `onFiles`: files from the OS, and
 * an image dragged out of the shelf (the cross-conversation staging area), which carries
 * only its shelf key — the `File` itself is still parked in the shelf store and is fetched
 * back on drop. That is deliberate: the shelf never uploads, so the bytes reach the daemon
 * exactly here, addressed to the bot the user dropped them on.
 */
export function useDropTarget(onFiles: (files: File[]) => void, disabled?: boolean) {
  const [over, setOver] = useState(false)
  // dragenter/dragleave fire for every child element; count them so the overlay does not
  // flicker as the pointer crosses the textarea.
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
      // A shelf drag exposes no `files`; resolve its keys back to the parked `File`s.
      const keys = shelfKeys(e)
      onFiles(keys.length ? shelfFilesFor(keys) : Array.from(e.dataTransfer?.files ?? []))
    },
  }

  return { over, props }
}

/** The "drop here" veil, rendered inside a `position: relative` composer. */
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

/** Pending thumbnails above the textarea. */
export function AttachTray({ items, onRemove, disabled }: { items: Pending[]; onRemove: (key: string) => void; disabled?: boolean }) {
  if (items.length === 0) return null
  return (
    <div className="attach-tray" role="list" aria-label="待送出的圖片">
      {items.map((it) => (
        <div key={it.key} className={`attach-thumb${it.error ? ' failed' : ''}${it.id ? '' : ' uploading'}`} role="listitem">
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

/** 📎 button that opens the file picker; images only. */
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
          // Reset so picking the same file twice still fires `change`.
          e.target.value = ''
        }}
      />
      <button
        type="button"
        className="icon-btn attach-pick icon-tip"
        disabled={disabled}
        aria-label="附加圖片"
        title="附加圖片（也可以直接拖進來或貼上）"
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

/**
 * A stored attachment's object URL, fetched once per id and shared across bubbles (the
 * same image can appear in a bot chat and in the group timeline).
 */
const urlCache = new Map<string, Promise<string>>()

function storedUrl(id: string): Promise<string> {
  let p = urlCache.get(id)
  if (!p) {
    p = api.attachmentUrl(id).catch((e: unknown) => {
      urlCache.delete(id)
      throw e
    })
    urlCache.set(id, p)
  }
  return p
}

/** Thumbnails on a sent message; click opens the full image. */
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
      // 讓縮圖上的選單／popover 先吃掉自己的 Escape，沒人處理才關 lightbox。
      if (e.key !== 'Escape' || e.defaultPrevented) return
      onClose()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [onClose])

  // 打開時焦點進到「關閉」鍵（圖片本身不可聚焦），關掉時退回原本按到的縮圖。
  useFocusTrap(true, boxRef, { initialFocus: () => closeRef.current })

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
