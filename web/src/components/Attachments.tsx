import { useT } from '../lib/i18n'

import { useEffect, useRef, useState } from 'react'
import { failureHint, failureLabel, loadFailure } from '../lib/attachmentLoad'
import type { AttachmentFailure } from '../lib/attachmentLoad'
import * as api from '../api'
import type { Attachment } from '../api/types'
import { createBlobUrlCache, type BlobLease } from '../lib/blobUrlCache'
import { useDialogFocus } from '../hooks/useDialogFocus'
import './attachments.css'
import { useImageViewer } from './useImageViewer'

import { formatSize, sizeLabel, type Pending } from './attachmentsHelpers'
import { progressLabel, uploadPercent } from './attachmentUpload'
import { useStore } from '../store/store'

export function DropVeil({ label }: { label?: string }) {
  const t = useT()
  const finalLabel = label ?? t('放開以附加檔案')
  return (
    <div className="drop-veil" aria-hidden="true">
      <span className="drop-veil-box">
        <FileIcon />
        {finalLabel}
      </span>
    </div>
  )
}

export function AttachTray({

  items,
  onRemove,
  onRetry,
  disabled,
}: {
  items: Pending[]
  onRemove: (key: string) => void
  onRetry?: (key: string) => void
  disabled?: boolean
}) {
  const t = useT()
  const [peek, setPeek] = useState<string | null>(null)
  if (items.length === 0) return null
  const peeked = items.find((it) => it.key === peek) ?? null
  const off = (key: string) => setPeek((k) => (k === key ? null : k))
  return (
    <div className="attach-tray" role="list" aria-label={t("待送出的檔案")}>
      {peeked?.isImage ? (
        <div className="attach-peek" aria-hidden="true">
          <img src={peeked.previewUrl} alt="" />
          <span className="attach-peek-cap">
            {peeked.name} · {sizeLabel(peeked.size, peeked.originalSize)}
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
          <span className="attach-thumb-text">
            <span className="attach-thumb-name" title={`${it.name} · ${sizeLabel(it.size, it.originalSize)}`}>
              {it.name}
            </span>
            <PendingState item={it} onRetry={onRetry} disabled={disabled} />
          </span>
          <button
            type="button"
            className="attach-thumb-x"
            aria-label={t("移除 {name}", { name: it.name })}
            title={t("移除")}
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

function PendingState({ item: it, onRetry, disabled }: { item: Pending; onRetry?: (key: string) => void; disabled?: boolean }) {
  const t = useT()
  if (it.id) return null
  if (it.error) {
    return (
      <span className="attach-thumb-state err">
        <span className="attach-thumb-err" title={it.error}>
          {t('失敗：{error}', { error: it.error })}
        </span>
        {onRetry && it.retryable ? (
          <button type="button" className="mini-btn attach-thumb-retry" disabled={disabled} aria-label={t("重試上傳 {name}", { name: it.name })} onClick={() => onRetry(it.key)}>
            {t("重試")}
          </button>
        ) : null}
      </span>
    )
  }
  if (it.compressing) return <span className="attach-thumb-state">{t('壓縮中…')}</span>
  const label = progressLabel(it.loaded, it.size)
  return (
    <>
      <span className="attach-thumb-state">{label}</span>
      <span
        className="attach-thumb-bar"
        role="progressbar"
        aria-label={t("上傳 {name}", { name: it.name })}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={uploadPercent(it.loaded, it.size)}
        aria-valuetext={label}
      >
        <span style={{ width: `${uploadPercent(it.loaded, it.size)}%` }} />
      </span>
    </>
  )
}

export function AttachPicker({
 onFiles, disabled }: { onFiles: (files: File[]) => void; disabled?: boolean }) {
  const t = useT()
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
        aria-label={t("附加檔案")}
        data-tip={t("附加檔案 · 拖放 / 貼上")}
        onClick={() => ref.current?.click()}
      >
        <FileIcon />
      </button>
    </>
  )
}

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

let urlCache: ReturnType<typeof createBlobUrlCache> | null = null

function storedUrl(id: string): BlobLease {
  urlCache ??= createBlobUrlCache(64)
  return urlCache.acquire(id, () => api.attachmentUrl(id))
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
  const { openImage, viewer } = useImageViewer()
  const { url, failure } = useStoredUrl(isImage ? item.id : null)
  return (
    <><button
      type="button"
      data-viewable-image={isImage || undefined}
      className={`msg-attachment${isImage ? '' : ' file'}${isImage && !url && !failure ? ' loading' : ''}${failure ? ' failed' : ''}`}
      title={`${item.name} · ${formatSize(item.size)}\n${item.path}${failure ? `\n${failureHint(failure)}` : ''}`}
      onClick={(event) => { if (!isImage || !openImage(event)) onOpen() }}
    >
      {isImage ? (
        url ? (
          <img src={url} alt={item.name} />
        ) : failure ? (
          <span className="msg-attachment-fallback">{failureLabel(failure)}</span>
        ) : (
          <span className="msg-attachment-fallback">…</span>
        )
      ) : (
        <>
          <FileIcon />
          <span className="msg-attachment-name">{item.name}</span>
        </>
      )}
    </button>{viewer}</>
  )
}

function useStoredUrl(id: string | null): { url: string | null; failure: AttachmentFailure | null } {
  const [state, setState] = useState<{ url: string | null; failure: AttachmentFailure | null }>({ url: null, failure: null })
  const [attempt, setAttempt] = useState(0)
  const online = useStore((s) => s.socket === 'open')
  const retryable = state.failure === 'error'
  useEffect(() => {
    if (online && retryable) setAttempt((n) => n + 1)
  }, [online, retryable])
  useEffect(() => {
    if (!id) return
    let live = true
    const lease = storedUrl(id)
    lease.url
      .then((u) => {
        if (live) setState({ url: u, failure: null })
      })
      .catch((e: unknown) => {
        if (live) setState({ url: null, failure: loadFailure(e) })
      })
    return () => {
      live = false
      lease.release()
    }
  }, [id, attempt])
  return state
}

function Lightbox({ item, onClose }: { item: Attachment; onClose: () => void }) {
  const t = useT()
  const isImage = item.mime.startsWith('image/')
  const { url, failure } = useStoredUrl(isImage ? item.id : null)
  const boxRef = useRef<HTMLDivElement>(null)
  const closeRef = useRef<HTMLButtonElement>(null)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
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
          ) : failure ? (
            <div className="lightbox-loading" role="status">
              {t('{label}：{hint}', { label: failureLabel(failure), hint: failureHint(failure) })}
            </div>
          ) : (
            <div className="lightbox-loading">{t('載入中…')}</div>
          )
        ) : (
          <div className="lightbox-file">
            <FileIcon />
            <span className="lightbox-file-name">{item.name}</span>
            <span className="lightbox-file-hint">{t('agent 讀得到下面這個路徑；這裡只說它放在哪。')}</span>
          </div>
        )}
        <div className="lightbox-bar">
          <span className="lightbox-name" title={item.path}>
            {item.name} · {formatSize(item.size)}
          </span>
          <code className="lightbox-path mono">{item.path}</code>
          <button type="button" className="mini-btn" ref={closeRef} onClick={onClose}>
            {t("關閉")}
          </button>
        </div>
      </div>
    </div>
  )
}
