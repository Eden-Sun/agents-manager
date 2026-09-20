/**
 * Attachments: uploaded first (`POST /bots/:id/attachments`), the send carries ids since agents only read text.
 * **Any file, not just images** (2026-09-14) — the mime only decides the card: image → thumbnail, else icon + name.
 * Bytes sit behind the UI token, so thumbnails are fetched into object URLs rather than a plain `src`.
 */

import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { Attachment } from '../api/types'
import { useDialogFocus } from '../hooks/useDialogFocus'
import './attachments.css'

import { formatSize, type Pending } from './attachmentsHelpers'

export function DropVeil({ label = '放開以附加檔案' }: { label?: string }) {
  return (
    <div className="drop-veil" aria-hidden="true">
      <span className="drop-veil-box">
        <FileIcon />
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
    <div className="attach-tray" role="list" aria-label="待送出的檔案">
      {peeked?.isImage ? (
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
          {it.isImage ? (
            <img src={it.previewUrl} alt={it.name} />
          ) : (
            <span className="attach-thumb-file" aria-hidden="true">
              <FileIcon />
            </span>
          )}
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
        aria-label="附加檔案"
        data-tip="附加檔案 · 拖放 / 貼上"
        onClick={() => ref.current?.click()}
      >
        <FileIcon />
      </button>
    </>
  )
}

/** A document glyph, for everything that is not an image. */
export function FileIcon() {
  return (
    <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
      <path
        d="M9.25 1.75H4.75a1.5 1.5 0 0 0-1.5 1.5v9.5a1.5 1.5 0 0 0 1.5 1.5h6.5a1.5 1.5 0 0 0 1.5-1.5V5.25z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinejoin="round"
      />
      <path d="M9.25 1.75v3.5h3.5" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
    </svg>
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
  const isImage = item.mime.startsWith('image/')
  // 非圖片不去抓位元組：一個 CSV 的縮圖沒有意義，看的是檔名。
  const url = useStoredUrl(isImage ? item.id : null)
  return (
    <button
      type="button"
      className={`msg-attachment${isImage ? '' : ' file'}${isImage && !url ? ' loading' : ''}`}
      title={`${item.name} · ${formatSize(item.size)}\n${item.path}`}
      onClick={onOpen}
    >
      {isImage ? (
        url ? (
          <img src={url} alt={item.name} />
        ) : (
          <span className="msg-attachment-fallback">…</span>
        )
      ) : (
        <>
          <FileIcon />
          <span className="msg-attachment-name">{item.name}</span>
        </>
      )}
    </button>
  )
}

function useStoredUrl(id: string | null): string | null {
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => {
    if (!id) return
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
  const isImage = item.mime.startsWith('image/')
  const url = useStoredUrl(isImage ? item.id : null)
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
        {isImage ? (
          url ? (
            <img src={url} alt={item.name} />
          ) : (
            <div className="lightbox-loading">載入中…</div>
          )
        ) : (
          <div className="lightbox-file">
            <FileIcon />
            <span className="lightbox-file-name">{item.name}</span>
            <span className="lightbox-file-hint">agent 讀得到下面這個路徑；這裡只說它放在哪。</span>
          </div>
        )}
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
