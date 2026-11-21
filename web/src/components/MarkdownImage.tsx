import { useT } from '../lib/i18n'
import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { readableImagePath } from '../lib/markdownUrl'
import { fetchRemoteImageBlob, isRemoteHttpImage, remoteImageInfo } from '../lib/remoteImage'
import './markdownImage.css'
import { useImageViewer } from './useImageViewer'

function isRemote(src: string): boolean {
  return /^(https?:|data:|blob:)/i.test(src)
}

function imageName(src: string): string {
  if (/^(data:|blob:)/i.test(src)) return 'image'
  const last = src.split(/[?#]/)[0].split(/[\\/]/).pop() ?? ''
  try {
    return decodeURIComponent(last) || 'image'
  } catch {
    return last || 'image'
  }
}

function RemoteImage({ src, alt }: { src: string; alt?: string }) {
  const t = useT()
  const info = remoteImageInfo(src)
  const [state, setState] = useState<'idle' | 'loading' | 'failed'>('idle')
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => () => {
    if (url) URL.revokeObjectURL(url)
  }, [url])
  const alive = useRef(true)
  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])
  if (url) {
    return (
      <ViewableImage src={url} alt={alt || imageName(src)} />
    )
  }
  const host = info?.host ?? t('未知網域')
  const load = () => {
    setState('loading')
    fetchRemoteImageBlob(src)
      .then((blob) => {
        const made = URL.createObjectURL(blob)
        if (alive.current) setUrl(made)
        else URL.revokeObjectURL(made)
      })
      .catch(() => setState('failed'))
  }
  return (
    <span className="md-image-remote" title={src}>
      <span aria-hidden="true">🖼</span> {alt ? t.x('外部圖片（{host}）：{alt}', { host: <strong>{host}</strong>, alt }) : t.x('外部圖片（{host}）', { host: <strong>{host}</strong> })}
      {info?.suspicious ? <span className="md-image-warn"> {t('網址帶了很長的參數或帳密，可能夾帶資料，確定要載入再點。')}</span> : null}
      {info?.internal ? <span className="md-image-warn"> {t('這是本機／內網位址：點下去會由這台機器對它發出請求。')}</span> : null}
      {state === 'failed' ? (
        <>
          <span className="md-image-warn"> {t('載入失敗（對方不允許跨站讀取、轉址到別的主機、不是支援的點陣圖或太大）。')}</span>
          <a href={src} target="_blank" rel="noreferrer noopener">
            {t('在新分頁開啟')}
          </a>
        </>
      ) : (
        <button
          type="button"
          className="mini-btn"
          disabled={state === 'loading'}
          onClick={(e) => {
            e.preventDefault()
            e.stopPropagation()
            load()
          }}
        >
          {state === 'loading' ? t('載入中…') : t('載入圖片')}
        </button>
      )}
    </span>
  )
}

export function MarkdownImage({
 botId, src, alt }: { botId: string | null | undefined; src?: string; alt?: string }) {
  const t = useT()
  const raw = typeof src === 'string' ? src : ''
  const local = raw !== '' && !isRemote(raw)
  const scope = `${botId ?? ''}\n${raw}`
  const [result, setResult] = useState<{ scope: string; url?: string; failed?: boolean }>({ scope: '' })
  const current = result.scope === scope ? result : null
  const url = current?.url ?? null
  const failed = current?.failed ?? false

  useEffect(() => {
    if (!local || !botId) return
    let alive = true
    let made: string | null = null
    api
      .localImageUrl(botId, raw)
      .then((u) => {
        made = u
        if (alive) setResult({ scope, url: u })
        else URL.revokeObjectURL(u)
      })
      .catch(() => {
        if (alive) setResult({ scope, failed: true })
      })
    return () => {
      alive = false
      if (made) URL.revokeObjectURL(made)
      setResult({ scope: '' })
    }
  }, [local, botId, raw, scope])

  if (!raw) return null
  if (!local) {
    if (isRemoteHttpImage(raw)) return <RemoteImage key={raw} src={raw} alt={alt} />
    return <ViewableImage src={raw} alt={alt || imageName(raw)} />
  }
  if (failed || !botId) {
    return (
      <span className="md-image-missing" title={t("圖片不在這個專案目錄裡、不是圖片檔，或檔案不存在")}>
        🖼 {alt ? t('{alt}：', { alt }) : ''}
        <code>{readableImagePath(raw)}</code>
      </span>
    )
  }
  if (!url) return <span className="md-image-loading">🖼 {alt || raw}</span>
  return (
    <ViewableImage src={url} alt={alt || imageName(raw)} />
  )
}

export function ViewableImage({ src, alt }: { src: string; alt: string }) {
  const { openImage, viewer } = useImageViewer()
  return <><a href={src} target="_blank" rel="noreferrer" className="md-image-link" data-viewable-image onClick={openImage}>
    <img className="md-image" src={src} alt={alt} loading="lazy" />
  </a>{viewer}</>
}
