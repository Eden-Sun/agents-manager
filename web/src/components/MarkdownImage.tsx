import { useEffect, useState } from 'react'
import * as api from '../api'
import { readableImagePath } from '../lib/markdownUrl'
import { fetchRemoteImageBlob, isRemoteHttpImage, remoteImageInfo } from '../lib/remoteImage'
import './markdownImage.css'

/** 網址型的圖片照常用 `<img>`；其他（`docs/x.png`、`/Users/…/x.png`、`file://…`）當成 bot 專案裡的檔案。 */
function isRemote(src: string): boolean {
  return /^(https?:|data:|blob:)/i.test(src)
}

/**
 * 遠端 http(s) 圖片預設不載入（issue #764）：先畫佔位（網域＋「載入圖片」），點了才抓這一張。
 * 抓的方式是 `fetch`→blob（`index.html` 的 CSP 只放 `img-src 'self' data: blob:`，`<img src="https://…">` 本來就載不了）；
 * 對方沒開 CORS 或抓不到，就改成「在新分頁開啟」的連結（那是使用者自己點的連結，不是自動請求）。
 */
function RemoteImage({ src, alt }: { src: string; alt?: string }) {
  const info = remoteImageInfo(src)
  const [state, setState] = useState<'idle' | 'loading' | 'failed'>('idle')
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => () => {
    if (url) URL.revokeObjectURL(url)
  }, [url])
  if (url) {
    return (
      <a href={url} target="_blank" rel="noreferrer" className="md-image-link">
        <img className="md-image" src={url} alt={alt ?? ''} />
      </a>
    )
  }
  const host = info?.host ?? '未知網域'
  const load = () => {
    setState('loading')
    fetchRemoteImageBlob(src)
      .then((blob) => setUrl(URL.createObjectURL(blob)))
      .catch(() => setState('failed'))
  }
  return (
    <span className="md-image-remote" title={src}>
      <span aria-hidden="true">🖼</span> 外部圖片（<strong>{host}</strong>）{alt ? `：${alt}` : ''}
      {info?.suspicious ? <span className="md-image-warn">　網址帶了很長的參數或帳密，可能夾帶資料，確定要載入再點。</span> : null}
      {state === 'failed' ? (
        <>
          <span className="md-image-warn">　載入失敗（對方不允許跨站讀取、不是圖片或太大）。</span>
          <a href={src} target="_blank" rel="noreferrer noopener">
            在新分頁開啟
          </a>
        </>
      ) : (
        <button type="button" className="mini-btn" disabled={state === 'loading'} onClick={load}>
          {state === 'loading' ? '載入中…' : '載入圖片'}
        </button>
      )}
    </span>
  )
}

/**
 * bot 回覆裡的 `![](docs/shot.png)`（2026-09-15 使用者截圖：只剩破圖）。路徑是 bot 工作目錄裡的檔，瀏覽器讀不到，
 * 改由 daemon 從該 bot 的專案目錄帶權杖讀出來（`GET /api/bots/{id}/local-image`）。讀不到就把路徑直接寫出來，不畫破圖。
 */
export function MarkdownImage({ botId, src, alt }: { botId: string | null | undefined; src?: string; alt?: string }) {
  const raw = typeof src === 'string' ? src : ''
  const local = raw !== '' && !isRemote(raw)
  const [url, setUrl] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    if (!local || !botId) return
    let alive = true
    let made: string | null = null
    api
      .localImageUrl(botId, raw)
      .then((u) => {
        made = u
        if (alive) setUrl(u)
        else URL.revokeObjectURL(u)
      })
      .catch(() => {
        if (alive) setFailed(true)
      })
    return () => {
      alive = false
      if (made) URL.revokeObjectURL(made)
    }
  }, [local, botId, raw])

  if (!raw) return null
  if (!local) {
    if (isRemoteHttpImage(raw)) return <RemoteImage src={raw} alt={alt} />
    return <img className="md-image" src={raw} alt={alt ?? ''} loading="lazy" />
  }
  if (failed || !botId) {
    return (
      <span className="md-image-missing" title="圖片不在這個專案目錄裡、不是圖片檔，或檔案不存在">
        🖼 {alt ? `${alt}：` : ''}
        <code>{readableImagePath(raw)}</code>
      </span>
    )
  }
  if (!url) return <span className="md-image-loading">🖼 {alt || raw}</span>
  return (
    <a href={url} target="_blank" rel="noreferrer" className="md-image-link">
      <img className="md-image" src={url} alt={alt ?? ''} />
    </a>
  )
}
